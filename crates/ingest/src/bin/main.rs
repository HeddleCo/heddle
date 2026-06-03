// SPDX-License-Identifier: Apache-2.0
//! `heddle-ingest` — import git history into a Heddle repository.
//!
//! The `import` subcommand covers the mechanical half of the import:
//! commits become states, trees and blobs round-trip byte-for-byte,
//! branches become threads, tags become markers, reflog-only commits
//! are included so force-pushed history isn't lost, and each reflog
//! entry is replayed into Heddle's oplog so `heddle undo` can reach past
//! the import boundary.
//!
//! The `reason` subcommand runs the reasoning pass: given a Heddle repo
//! that was already imported (so the sha-map exists), it locates chat
//! transcripts on disk, matches each session to a commit, extracts
//! prescriptive notecards ("Rule", "Gotcha", "Why", "Migration") and
//! lands them as context annotations on the corresponding states. You
//! can point this at your own repo + `~/.claude` / `~/.codex` stores;
//! everything runs locally, no network round-trip.
//!
//! Typical flow (`--heddle` points at the worktree root; the `.heddle`
//! subdirectory is created inside it):
//!
//! ```bash
//! heddle-ingest import --git /path/to/repo --heddle /path/to/repo
//! heddle-ingest reason --git /path/to/repo --heddle /path/to/repo
//! ```

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use ingest::{
    GitSource, ImportOptions, ReasoningPipeline, ReasoningPipelineParams, Result, ShaMap,
    TranscriptRoots, import_git_into_with_options, load_transcripts, pipeline_default_commits,
};
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "heddle-ingest",
    about = "Import git history into a Heddle repository",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Increase log verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect or query the SHA map sidecar.
    Map {
        /// Path to the sidecar SQLite file (usually `<heddle_dir>/ingest/sha_map.sqlite`).
        path: PathBuf,
        #[command(subcommand)]
        action: MapAction,
    },
    /// Run the mechanical import (commits → states, refs → threads,
    /// reflog → oplog).
    Import {
        /// Path to the source git repository.
        #[arg(long)]
        git: PathBuf,
        /// Path to the target Heddle worktree root (the directory that will
        /// contain `.heddle`). A trailing `.heddle` component is tolerated for
        /// backwards compatibility with older help text.
        #[arg(long)]
        heddle: PathBuf,
        /// Accept git tree entries Heddle cannot represent losslessly.
        ///
        /// By default import fails on the first unrepresentable tree entry.
        /// With this flag, import restores the historical drop behavior and
        /// prints an end-of-run summary of every affected entry.
        #[arg(long)]
        lossy: bool,
    },
    /// Mine agent chat transcripts for reasoning annotations and attach
    /// them to the matching imported states. Requires `import` to have
    /// already been run (needs the SHA map).
    Reason {
        /// Path to the git repository the transcripts are about.
        #[arg(long)]
        git: PathBuf,
        /// Path to the Heddle worktree root produced by `heddle-ingest import`
        /// (the directory that contains `.heddle`). A trailing `.heddle`
        /// component is tolerated for backwards compatibility.
        #[arg(long)]
        heddle: PathBuf,
        /// Only process this commit SHA. Repeat to process a small
        /// explicit set. When omitted, every commit currently in the
        /// SHA map is considered (optionally narrowed by `--since` /
        /// `--limit`).
        #[arg(long = "commit", value_name = "SHA")]
        commits: Vec<String>,
        /// Cap the number of commits processed. Applied *after*
        /// `--since`. Useful for iterating on extraction knobs without
        /// paying for a whole-repo pass.
        #[arg(long)]
        limit: Option<usize>,
        /// Skip commits authored before this ISO-8601 timestamp
        /// (e.g. `2024-01-01T00:00:00Z`). Useful for incremental runs
        /// that already know their last-seen baseline.
        #[arg(long)]
        since: Option<DateTime<Utc>>,
        /// Override the Claude transcript store. Default: `$HOME/.claude`.
        /// Set to an empty string to disable Claude discovery entirely.
        #[arg(long = "claude-home", value_name = "PATH")]
        claude_home: Option<String>,
        /// Override the Codex transcript store. Default: `$HOME/.codex`.
        /// Set to an empty string to disable Codex discovery entirely.
        #[arg(long = "codex-home", value_name = "PATH")]
        codex_home: Option<String>,
        /// Override the OpenCode data directory (the one that holds
        /// `opencode.db`). Default: `$HOME/.local/share/opencode`.
        /// Set to an empty string to disable OpenCode discovery entirely.
        #[arg(long = "opencode-home", value_name = "PATH")]
        opencode_home: Option<String>,
        /// Only load Codex rollouts authored after this ISO-8601
        /// timestamp. The Codex store is date-sharded and often large;
        /// pinning this narrows the scan cost.
        #[arg(long)]
        codex_since: Option<DateTime<Utc>>,
        /// Matcher cap: mine only the top-N transcript sessions per
        /// commit after ranking. Higher = more coverage at the cost of
        /// more cross-attribution noise. Defaults to `5`, which on a
        /// dogfooded Heddle-sized repo (~400 commits, ~600 sessions)
        /// produced ~1k notecards across ~50 states. Lower this to `2`
        /// for tighter precision; raise to `10` only on a narrowly
        /// scoped commit list.
        #[arg(long, default_value_t = 5)]
        max_sessions_per_commit: usize,
        /// Matcher floor: drop sessions whose confidence falls below
        /// this before extraction runs. Confidence combines file overlap
        /// (0.65 weight), time fit (0.25), and Co-Authored-By hint (0.10) —
        /// see `crates/heddle-ingest/src/transcript/matcher.rs`. The
        /// previous default of `0.40` was tuned for "no false positives
        /// ever" and produced zero matches on a typical real-world repo.
        /// `0.20` lets borderline-but-correct matches through; tighten
        /// to `0.40` if mis-attribution becomes a problem.
        #[arg(long, default_value_t = 0.20)]
        min_match_confidence: f32,
        /// Report what would happen without writing any annotations.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MapAction {
    /// Print summary statistics.
    Stats,
    /// Resolve a git SHA to its Heddle identifier.
    LookupGit { sha: String },
    /// Resolve a Heddle identifier back to its git SHA.
    LookupHeddle { heddle: String },
}

fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Map { path, action } => run_map(&path, action),
        Command::Import { git, heddle, lossy } => run_import(&git, &heddle, lossy),
        Command::Reason {
            git,
            heddle,
            commits,
            limit,
            since,
            claude_home,
            codex_home,
            opencode_home,
            codex_since,
            max_sessions_per_commit,
            min_match_confidence,
            dry_run,
        } => run_reason(ReasonArgs {
            git: &git,
            heddle: &heddle,
            commits,
            limit,
            since,
            claude_home: claude_home.as_deref(),
            codex_home: codex_home.as_deref(),
            opencode_home: opencode_home.as_deref(),
            codex_since,
            max_sessions_per_commit,
            min_match_confidence,
            dry_run,
        }),
    }
}

fn run_import(
    git_path: &std::path::Path,
    heddle_path: &std::path::Path,
    lossy: bool,
) -> Result<()> {
    // `import_git_into` handles init-vs-open, the sha-map sidecar, and
    // every walker/writer in the right order. We just surface the stats.
    let (stats, _map) =
        import_git_into_with_options(git_path, heddle_path, ImportOptions { lossy })?;
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
    println!("refs:");
    println!("  walked: {walked}  kept: {kept}  ignored: {ignored}",);
    println!("    local branches:  {}", r.local_branches);
    println!("    tags:            {}", r.tags);
    println!("    remote branches: {}", r.remote_branches);
    if r.symbolic_skipped > 0 {
        println!(
            "    symbolic refs:   {} ignored (e.g. origin/HEAD)",
            r.symbolic_skipped,
        );
    }
    if r.peel_failed > 0 {
        println!(
            "    peel-failed:     {} ignored (dangling targets)",
            r.peel_failed,
        );
    }
    if r.non_commit_skipped > 0 {
        println!(
            "    non-commit refs: {} ignored (e.g. annotated tag → blob/tree, like junio-gpg-pub)",
            r.non_commit_skipped,
        );
    }
    println!("commits:");
    println!("  imported: {}", stats.commits_imported);
    println!("    reflog-only: {}", stats.reflog_only_commits);
    println!("trees:  {}", stats.trees_imported);
    println!("blobs:  {}", stats.blobs_imported);
    println!("threads written:  {}", stats.refs.threads_written);
    println!("markers written:  {}", stats.refs.markers_written);
    if !stats.lossy_entries.is_empty() {
        println!(
            "lossy import accepted for {} tree entries:",
            stats.lossy_entries.len()
        );
        for entry in &stats.lossy_entries {
            println!("  {}", entry.summary_line());
        }
    }
    // Oplog block: only rendered when the honest-history pass actually
    // ran (all counters zero ⇒ either disabled or no-op repo). Shows up
    // below the refs because it's a downstream derivative of them.
    let op = &stats.oplog;
    let oplog_total = op.gotos
        + op.thread_creates
        + op.thread_updates
        + op.thread_deletes
        + op.marker_creates
        + op.marker_deletes;
    if oplog_total > 0 {
        println!("  oplog ops:        {}", oplog_total);
        println!("    thread create:  {}", op.thread_creates);
        println!("    thread update:  {}", op.thread_updates);
        println!("    thread delete:  {}", op.thread_deletes);
        println!("    marker create:  {}", op.marker_creates);
        println!("    marker delete:  {}", op.marker_deletes);
        println!("    goto:           {}", op.gotos);
    }
    if stats.refs.skipped_unmapped > 0 {
        // A non-zero skip count means a ref pointed at a commit we
        // couldn't translate — surface it as a warning exit code.
        eprintln!(
            "warning: {} refs skipped (target commit not in sha map)",
            stats.refs.skipped_unmapped
        );
    }
    if stats.oplog.skipped_unmapped > 0 {
        eprintln!(
            "warning: {} reflog entries skipped (target commit not in sha map)",
            stats.oplog.skipped_unmapped
        );
    }
    info!("import complete");
    Ok(())
}

/// Bundled arguments for `run_reason`, split out because the option
/// surface is wide enough that a positional call site would be hard to
/// read at a glance.
struct ReasonArgs<'a> {
    git: &'a Path,
    heddle: &'a Path,
    commits: Vec<String>,
    limit: Option<usize>,
    since: Option<DateTime<Utc>>,
    claude_home: Option<&'a str>,
    codex_home: Option<&'a str>,
    opencode_home: Option<&'a str>,
    codex_since: Option<DateTime<Utc>>,
    max_sessions_per_commit: usize,
    min_match_confidence: f32,
    dry_run: bool,
}

fn run_reason(args: ReasonArgs<'_>) -> Result<()> {
    // Open the sha-map sidecar. We refuse to run if the map is missing;
    // `reason` is strictly a post-pass to `import`, and a missing map
    // almost always means the user forgot the first step.
    let repo = repo::Repository::open(args.heddle)?;
    let map_path = repo.heddle_dir().join("ingest").join("sha_map.sqlite");
    if !map_path.exists() {
        eprintln!(
            "error: no sha map at {}.\n\
             run `heddle-ingest import --git {} --heddle {}` first.",
            map_path.display(),
            args.git.display(),
            args.heddle.display(),
        );
        std::process::exit(2);
    }
    let map = ShaMap::open(&map_path)?;
    let git = GitSource::open(args.git)?;

    // Discover transcripts. An empty-string override disables that
    // provider — handy for CI/tests where you want hermetic behaviour,
    // or for users who only use one agent.
    let roots = build_transcript_roots(
        args.claude_home,
        args.codex_home,
        args.opencode_home,
        args.codex_since,
    );
    let transcripts = load_transcripts(args.git, &roots);
    let display_root = |p: Option<&PathBuf>| -> String {
        p.map(|p| p.display().to_string())
            .unwrap_or_else(|| "<disabled>".into())
    };
    println!(
        "loaded {} transcripts (claude_home={}, codex_home={}, opencode_home={})",
        transcripts.len(),
        display_root(roots.claude.as_ref()),
        display_root(roots.codex.as_ref()),
        display_root(roots.opencode_home.as_ref()),
    );
    if transcripts.is_empty() {
        eprintln!(
            "warning: no transcripts found for {}. \
             check that the transcript stores exist and contain \
             sessions whose cwd is (or is under) this repo.",
            args.git.display()
        );
    }

    // Build the commit list. `--commit` overrides everything. Otherwise
    // we take the default (every mapped commit), filter by `--since`,
    // and cap by `--limit`.
    let mut commits = if args.commits.is_empty() {
        pipeline_default_commits(&map)
    } else {
        args.commits.clone()
    };

    if let Some(since) = args.since {
        commits.retain(|sha| match git.read_commit(sha) {
            Ok(c) => c.authored_at >= since,
            Err(_) => {
                eprintln!("warning: can't read commit {sha} for --since filter");
                false
            }
        });
    }

    if let Some(limit) = args.limit {
        commits.truncate(limit);
    }

    println!("processing {} commits", commits.len());

    // Now run the pipeline.
    let mut params = ReasoningPipelineParams {
        max_sessions_per_commit: args.max_sessions_per_commit,
        min_match_confidence: args.min_match_confidence,
        ..ReasoningPipelineParams::default()
    };
    if args.dry_run {
        params.emit_annotations = false;
    }
    let mut pipeline =
        ReasoningPipeline::new(&repo, &git, &map, args.git, transcripts).with_params(params);
    let stats = pipeline.run(&commits)?;

    if args.dry_run {
        println!("dry-run: not writing annotations");
    }
    println!("reasoning pass complete");
    println!("  commits scanned:        {}", stats.commits_scanned);
    println!("  commits with matches:   {}", stats.commits_with_matches);
    println!("  sessions mined:         {}", stats.sessions_mined);
    println!("  points extracted:       {}", stats.points_extracted);
    println!(
        "  points rejected:        {}",
        stats.points_rejected_quality
    );
    println!("  points deduped (cross): {}", stats.points_deduped);
    println!("  states updated:         {}", stats.emit.states_updated);
    println!(
        "  annotations written:    {}",
        stats.emit.annotations_written
    );
    if args.dry_run && !pipeline.preview().is_empty() {
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
    if stats.emit.deduped > 0 {
        println!("  annotations deduped:    {}", stats.emit.deduped);
    }
    if stats.skipped_untranslated_tree > 0 {
        println!(
            "  skipped (no tree map):  {}",
            stats.skipped_untranslated_tree
        );
    }
    if stats.skipped_git_errors > 0 {
        println!("  skipped (git errors):   {}", stats.skipped_git_errors);
    }
    if stats.emit.skipped_missing_state + stats.emit.skipped_malformed > 0 {
        println!(
            "  skipped (emit):         missing_state={}, malformed={}",
            stats.emit.skipped_missing_state, stats.emit.skipped_malformed
        );
    }
    if stats.emit.states_updated > 0 {
        // Tell the user where to look. Without this nudge, `heddle context
        // list` against the bare repo returns `[]` (annotations live on
        // the commits they're *about*, not on HEAD), which makes the
        // import look like a no-op even after a successful pass.
        println!();
        println!(
            "annotations attached to {} states. To browse them:",
            stats.emit.states_updated
        );
        println!(
            "  heddle --repo {} log              # find a state id",
            args.heddle.display()
        );
        println!(
            "  heddle --repo {} context list --ref <state-id>",
            args.heddle.display(),
        );
        println!("or open the web app at /app/repo/<repo>/-/files/<path> to see them inline.");
    } else if stats.commits_with_matches > 0 {
        println!();
        println!(
            "matched {} commits but extracted no notecards. Try lowering --min-match-confidence \
             below {:.2} or raising --max-sessions-per-commit above {}.",
            stats.commits_with_matches, args.min_match_confidence, args.max_sessions_per_commit,
        );
    } else {
        println!();
        println!(
            "no commits matched any session. Either no transcripts touched this repo, \
             or --min-match-confidence ({:.2}) is too strict for this corpus.",
            args.min_match_confidence,
        );
    }
    info!("reason complete");
    Ok(())
}

/// Translate CLI overrides into a [`TranscriptRoots`]. An empty string
/// ("") disables the provider; `None` falls back to the default
/// (`$HOME/.claude` / `$HOME/.codex` / `$HOME/.local/share/opencode`).
fn build_transcript_roots(
    claude_override: Option<&str>,
    codex_override: Option<&str>,
    opencode_override: Option<&str>,
    codex_since: Option<DateTime<Utc>>,
) -> TranscriptRoots {
    let default = TranscriptRoots::default();
    let resolve = |ovr: Option<&str>, fallback: Option<PathBuf>| match ovr {
        Some("") => None,
        Some(s) => Some(PathBuf::from(s)),
        None => fallback,
    };
    TranscriptRoots {
        claude: resolve(claude_override, default.claude),
        codex: resolve(codex_override, default.codex),
        opencode_home: resolve(opencode_override, default.opencode_home),
        codex_since,
    }
}

fn run_map(path: &std::path::Path, action: MapAction) -> Result<()> {
    let map = ShaMap::open(path)?;
    match action {
        MapAction::Stats => {
            println!("records: {}", map.len());
            println!("commits: {}", map.commit_count());
            println!("path: {}", path.display());
        }
        MapAction::LookupGit { sha } => {
            if let Some(cid) = map.get_commit(&sha) {
                println!("commit {sha} → {}", cid.to_string_full());
            } else if let Some(h) = map.get_tree(&sha) {
                println!("tree   {sha} → {}", h.to_hex());
            } else if let Some(h) = map.get_blob(&sha) {
                println!("blob   {sha} → {}", h.to_hex());
            } else {
                eprintln!("no mapping for git sha {sha}");
                std::process::exit(1);
            }
        }
        MapAction::LookupHeddle { heddle } => match map.get_git_for_heddle(&heddle) {
            Some(sha) => println!("{heddle} → git {sha}"),
            None => {
                eprintln!("no git sha mapped to {heddle}");
                std::process::exit(1);
            }
        },
    }
    info!("map op complete");
    Ok(())
}

fn init_tracing(verbosity: u8) {
    let filter = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .without_time()
        .try_init();
}
