// SPDX-License-Identifier: Apache-2.0
//! Bridge command implementations.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use repo::Repository;
use serde::Serialize;

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

struct ImportProgress {
    enabled: bool,
    current: usize,
    total: usize,
}

impl ImportProgress {
    fn start(cli: &Cli, repo: &Repository, scope: &str, source_label: &str) -> Self {
        let enabled = !should_output_json(cli, Some(repo.config()));
        if enabled {
            println!(
                "{} {} from {}",
                style::dim("Importing Git history:"),
                scope,
                style::dim(source_label)
            );
        }
        let progress = Self {
            enabled,
            current: 0,
            total: 3,
        };
        progress.step("scanning refs");
        progress
    }

    fn step(&self, label: &str) {
        if !self.enabled {
            return;
        }
        let next = self.current + 1;
        if io::stdout().is_terminal() {
            print!(
                "\r{}",
                style::dim(&format!("[{next}/{}] {label}...", self.total))
            );
            io::stdout().flush().ok();
        } else {
            println!(
                "{}",
                style::dim(&format!("[{next}/{}] {label}", self.total))
            );
        }
    }

    fn advance(&mut self, label: &str) {
        self.current += 1;
        self.step(label);
    }

    fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        self.current = self.total;
        if io::stdout().is_terminal() {
            print!("\r{}\n", style::accent("[done] imported Git history"));
            io::stdout().flush().ok();
        } else {
            println!("{}", style::accent("[done] imported Git history"));
        }
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

/// Wire shape for `heddle bridge git status --json`. This is the
/// canonical surface for import-hint information; other `--json`
/// outputs no longer include it. Optional fields are emitted as
/// explicit `null` rather than omitted, matching the discipline used
/// across the CLI's JSON outputs.
#[derive(Serialize)]
struct BridgeGitStatusOutput {
    repository_capability: String,
    storage_model: String,
    /// Path on disk to the bridge mirror, when initialized.
    mirror_path: Option<String>,
    /// `true` when `.heddle/git` has been seeded with a mirror.
    mirror_initialized: bool,
    /// `Some(...)` when one or more local Git branches exist that
    /// haven't been imported yet. `None` when the bridge is in sync.
    git_overlay_import_hint: Option<BridgeGitImportHintOutput>,
}

#[derive(Serialize)]
struct BridgeGitImportHintOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

fn cmd_bridge_git_status(cli: &Cli, repo: &Repository) -> Result<()> {
    let bridge = GitBridge::new(repo);
    let mirror_path = bridge.mirror_path().to_path_buf();
    let mirror_initialized = mirror_path.exists();
    let import_hint = repo.git_overlay_import_hint().unwrap_or(None);
    let output = BridgeGitStatusOutput {
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
        "Repository mode: {} ({})",
        output.repository_capability, output.storage_model
    );
    if output.mirror_initialized {
        println!(
            "Mirror: {} (initialized)",
            style::dim(output.mirror_path.as_deref().unwrap_or(""))
        );
    } else {
        println!(
            "Mirror: {} (not initialized — run `heddle bridge git init`)",
            style::dim(output.mirror_path.as_deref().unwrap_or(""))
        );
    }
    match &output.git_overlay_import_hint {
        Some(hint) => {
            println!(
                "Git import: {} branch(es) still live only in Git ({})",
                hint.missing_branch_count,
                crate::cli::render::preview_list(&hint.missing_branches, hint.missing_branch_count,)
            );
            println!("Next step: {}", style::bold(&hint.recommended_command));
        }
        None => println!("Git import: in sync"),
    }
}

/// Execute bridge subcommands.
pub fn cmd_bridge_git(cli: &Cli, command: GitCommands) -> Result<()> {
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

            let already_in_sync =
                stats.states_created == 0 && stats.commits_imported > 0;
            if should_output_json(cli, Some(repo.config())) {
                let out = serde_json::json!({
                    "commits_imported": stats.commits_imported,
                    "states_created": stats.states_created,
                    "branches_synced": stats.branches_synced,
                    "tags_synced": stats.tags_synced,
                    "skipped_non_commit_refs": stats.skipped_non_commit_refs.len(),
                    "partial_mirror_refs": stats.partial_mirror_refs.len(),
                    "already_in_sync": already_in_sync,
                });
                println!("{out}");
            } else {
                if already_in_sync {
                    println!(
                        "{} already in sync with {} — every commit was \
                         already imported",
                        style::ok_marker(),
                        style::dim(&source_label)
                    );
                } else {
                    println!(
                        "{} imported Git history from {}",
                        style::ok_marker(),
                        style::dim(&source_label)
                    );
                }
                println!(
                    "  {}",
                    style::field("commits", &style::bold(&stats.commits_imported.to_string()))
                );
                println!(
                    "  {}",
                    style::field(
                        "states created",
                        &style::bold(&stats.states_created.to_string())
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "branches",
                        &format!(
                            "{} synced to threads",
                            style::bold(&stats.branches_synced.to_string())
                        )
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "tags",
                        &format!(
                            "{} synced to markers",
                            style::bold(&stats.tags_synced.to_string())
                        )
                    )
                );
                if !stats.skipped_non_commit_refs.is_empty() {
                    println!(
                        "{} skipped {} non-commit-pointing refs",
                        style::warn_marker(),
                        style::bold(&stats.skipped_non_commit_refs.len().to_string())
                    );
                }
                if !stats.partial_mirror_refs.is_empty() {
                    println!(
                        "{} partial mirror for {} refs; SHA-stable export degraded",
                        style::warn_marker(),
                        style::bold(&stats.partial_mirror_refs.len().to_string())
                    );
                }
            }
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
            if should_output_json(cli, Some(repo.config())) {
                let out = serde_json::json!({
                    "states_exported": export_stats.states_exported,
                    "commits_imported": sync_commits_imported,
                    "threads_synced": export_stats.threads_synced + import_stats.branches_synced,
                    "markers_synced": export_stats.markers_synced + import_stats.tags_synced,
                });
                println!("{out}");
            } else {
                println!("{} synced Git overlay", style::ok_marker());
                println!(
                    "  {}",
                    style::field(
                        "exported",
                        &style::count(export_stats.states_exported, "state")
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "imported",
                        &style::count(sync_commits_imported, "commit")
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "threads",
                        &format!(
                            "{} synced with branches",
                            style::bold(
                                &(export_stats.threads_synced + import_stats.branches_synced)
                                    .to_string()
                            )
                        )
                    )
                );
                println!(
                    "  {}",
                    style::field(
                        "markers",
                        &format!(
                            "{} synced with tags",
                            style::bold(
                                &(export_stats.markers_synced + import_stats.tags_synced)
                                    .to_string()
                            )
                        )
                    )
                );
            }
        }

        GitCommands::Push { remote } => {
            let remote_name = remote.as_deref().unwrap_or("origin");
            bridge.push(remote_name)?;

            if should_output_json(cli, Some(repo.config())) {
                let out = serde_json::json!({
                    "pushed": true,
                    "remote": remote_name,
                });
                println!("{out}");
            } else {
                println!(
                    "{} pushed to remote {}",
                    style::ok_marker(),
                    style::bold(remote_name)
                );
            }
        }

        GitCommands::Pull { remote } => {
            let remote_name = remote.as_deref().unwrap_or("origin");
            bridge.pull(remote_name)?;

            if should_output_json(cli, Some(repo.config())) {
                let out = serde_json::json!({
                    "pulled": true,
                    "remote": remote_name,
                });
                println!("{out}");
            } else {
                println!(
                    "{} pulled from remote {}",
                    style::ok_marker(),
                    style::bold(remote_name)
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
        anyhow::bail!(
            "no sha map at {}. Run `heddle bridge ingest --path {}` first.",
            map_path.display(),
            git_path.display(),
        );
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
