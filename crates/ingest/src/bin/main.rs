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

use std::{
    collections::VecDeque,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use ingest::{
    import_git_into_scoped_with_options, load_transcripts, pipeline_default_commits, GitSource,
    ImportOptions, ImportScope, IngestError, ReasoningPipeline, ReasoningPipelineParams, Result,
    ShaMap, TranscriptRoots,
};
use tracing::info;

#[derive(Debug)]
struct Cli {
    command: Command,
    verbose: u8,
}

#[derive(Debug)]
enum Command {
    Map {
        path: PathBuf,
        action: MapAction,
    },
    Import {
        git: PathBuf,
        heddle: PathBuf,
        lossy: bool,
        refs: Vec<String>,
    },
    Reason {
        git: PathBuf,
        heddle: PathBuf,
        commits: Vec<String>,
        limit: Option<usize>,
        since: Option<DateTime<Utc>>,
        claude_home: Option<String>,
        codex_home: Option<String>,
        opencode_home: Option<String>,
        codex_since: Option<DateTime<Utc>>,
        max_sessions_per_commit: usize,
        min_match_confidence: f32,
        dry_run: bool,
    },
}

#[derive(Debug)]
enum MapAction {
    Stats,
    LookupGit { sha: String },
    LookupHeddle { heddle: String },
}

impl Cli {
    fn parse() -> Result<Self> {
        let mut verbose = 0u8;
        let mut args = VecDeque::new();

        for arg in env::args_os().skip(1) {
            if arg == "-h" || arg == "--help" {
                print_usage();
                std::process::exit(0);
            }
            if let Some(count) = verbose_count(&arg) {
                verbose = verbose.saturating_add(count);
            } else {
                args.push_back(arg);
            }
        }

        let command = parse_command(&mut args)?;
        if let Some(extra) = args.pop_front() {
            return Err(parse_error(format!(
                "unexpected argument '{}'",
                display_arg(&extra)
            )));
        }

        Ok(Self { command, verbose })
    }
}

fn parse_command(args: &mut VecDeque<OsString>) -> Result<Command> {
    let command = next_string(args, "command")?;
    match command.as_str() {
        "map" => parse_map(args),
        "import" => parse_import(args),
        "reason" => parse_reason(args),
        other => Err(parse_error(format!("unknown command '{other}'"))),
    }
}

fn parse_map(args: &mut VecDeque<OsString>) -> Result<Command> {
    let path = next_path(args, "map path")?;
    let action = next_string(args, "map action")?;
    let action = match action.as_str() {
        "stats" => MapAction::Stats,
        "lookup-git" => MapAction::LookupGit {
            sha: next_string(args, "git sha")?,
        },
        "lookup-heddle" => MapAction::LookupHeddle {
            heddle: next_string(args, "heddle id")?,
        },
        other => return Err(parse_error(format!("unknown map action '{other}'"))),
    };
    Ok(Command::Map { path, action })
}

fn parse_import(args: &mut VecDeque<OsString>) -> Result<Command> {
    let mut git = None;
    let mut heddle = None;
    let mut lossy = false;
    let mut refs = Vec::new();

    while let Some(arg) = args.pop_front() {
        let Some(arg_str) = arg.to_str() else {
            return Err(parse_error(format!(
                "option name must be valid UTF-8: '{}'",
                display_arg(&arg)
            )));
        };
        match option_name(arg_str) {
            "--git" => git = Some(option_path(args, arg_str, "--git")?),
            "--heddle" => heddle = Some(option_path(args, arg_str, "--heddle")?),
            "--lossy" => lossy = true,
            "--ref" => refs.push(option_string(args, arg_str, "--ref")?),
            _ => return Err(parse_error(format!("unknown import option '{arg_str}'"))),
        }
    }

    Ok(Command::Import {
        git: git.ok_or_else(|| parse_error("missing required option --git"))?,
        heddle: heddle.ok_or_else(|| parse_error("missing required option --heddle"))?,
        lossy,
        refs,
    })
}

fn parse_reason(args: &mut VecDeque<OsString>) -> Result<Command> {
    let mut git = None;
    let mut heddle = None;
    let mut commits = Vec::new();
    let mut limit = None;
    let mut since = None;
    let mut claude_home = None;
    let mut codex_home = None;
    let mut opencode_home = None;
    let mut codex_since = None;
    let mut max_sessions_per_commit = 5;
    let mut min_match_confidence = 0.20;
    let mut dry_run = false;

    while let Some(arg) = args.pop_front() {
        let Some(arg_str) = arg.to_str() else {
            return Err(parse_error(format!(
                "option name must be valid UTF-8: '{}'",
                display_arg(&arg)
            )));
        };
        match option_name(arg_str) {
            "--git" => git = Some(option_path(args, arg_str, "--git")?),
            "--heddle" => heddle = Some(option_path(args, arg_str, "--heddle")?),
            "--commit" => commits.push(option_string(args, arg_str, "--commit")?),
            "--limit" => {
                limit = Some(parse_value(
                    "--limit",
                    option_string(args, arg_str, "--limit")?,
                )?)
            }
            "--since" => {
                since = Some(parse_value(
                    "--since",
                    option_string(args, arg_str, "--since")?,
                )?)
            }
            "--claude-home" => claude_home = Some(option_string(args, arg_str, "--claude-home")?),
            "--codex-home" => codex_home = Some(option_string(args, arg_str, "--codex-home")?),
            "--opencode-home" => {
                opencode_home = Some(option_string(args, arg_str, "--opencode-home")?)
            }
            "--codex-since" => {
                codex_since = Some(parse_value(
                    "--codex-since",
                    option_string(args, arg_str, "--codex-since")?,
                )?)
            }
            "--max-sessions-per-commit" => {
                max_sessions_per_commit = parse_value(
                    "--max-sessions-per-commit",
                    option_string(args, arg_str, "--max-sessions-per-commit")?,
                )?
            }
            "--min-match-confidence" => {
                min_match_confidence = parse_value(
                    "--min-match-confidence",
                    option_string(args, arg_str, "--min-match-confidence")?,
                )?
            }
            "--dry-run" => dry_run = true,
            _ => return Err(parse_error(format!("unknown reason option '{arg_str}'"))),
        }
    }

    Ok(Command::Reason {
        git: git.ok_or_else(|| parse_error("missing required option --git"))?,
        heddle: heddle.ok_or_else(|| parse_error("missing required option --heddle"))?,
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
    })
}

fn verbose_count(arg: &OsString) -> Option<u8> {
    let arg = arg.to_str()?;
    if arg == "--verbose" {
        return Some(1);
    }
    if arg.len() > 1 && arg.starts_with('-') && arg[1..].chars().all(|c| c == 'v') {
        return Some((arg.len() - 1).try_into().unwrap_or(u8::MAX));
    }
    None
}

fn option_name(arg: &str) -> &str {
    arg.split_once('=').map_or(arg, |(name, _)| name)
}

fn option_path(args: &mut VecDeque<OsString>, arg: &str, name: &str) -> Result<PathBuf> {
    if let Some((_, value)) = arg.split_once('=') {
        return Ok(PathBuf::from(value));
    }
    Ok(PathBuf::from(next_os(args, name)?))
}

fn option_string(args: &mut VecDeque<OsString>, arg: &str, name: &str) -> Result<String> {
    if let Some((_, value)) = arg.split_once('=') {
        return Ok(value.to_string());
    }
    next_string(args, name)
}

fn next_path(args: &mut VecDeque<OsString>, label: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(next_os(args, label)?))
}

fn next_os(args: &mut VecDeque<OsString>, label: &str) -> Result<OsString> {
    args.pop_front()
        .ok_or_else(|| parse_error(format!("missing {label}")))
}

fn next_string(args: &mut VecDeque<OsString>, label: &str) -> Result<String> {
    let value = next_os(args, label)?;
    value.into_string().map_err(|value| {
        parse_error(format!(
            "{label} must be valid UTF-8: '{}'",
            display_arg(&value)
        ))
    })
}

fn parse_value<T>(name: &str, value: String) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|err| parse_error(format!("invalid value for {name}: {err}")))
}

fn display_arg(arg: &OsString) -> String {
    arg.to_string_lossy().into_owned()
}

fn parse_error(message: impl Into<String>) -> IngestError {
    IngestError::Other(message.into())
}

fn print_usage() {
    println!(
        "\
heddle-ingest

Usage:
  heddle-ingest [-v|-vv|--verbose] map <path> <stats|lookup-git|lookup-heddle> [value]
  heddle-ingest [-v|-vv|--verbose] import --git <path> --heddle <path> [--lossy] [--ref <ref>]...
  heddle-ingest [-v|-vv|--verbose] reason --git <path> --heddle <path> [options]

Reason options:
  --commit <sha>...
  --limit <n>
  --since <timestamp>
  --claude-home <path>
  --codex-home <path>
  --opencode-home <path>
  --codex-since <timestamp>
  --max-sessions-per-commit <n>
  --min-match-confidence <float>
  --dry-run"
    );
}

fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse()?;
    init_tracing(cli.verbose);

    match cli.command {
        Command::Map { path, action } => run_map(&path, action),
        Command::Import {
            git,
            heddle,
            lossy,
            refs,
        } => run_import(&git, &heddle, lossy, &refs),
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
    refs: &[String],
) -> Result<()> {
    // `import_git_into` handles init-vs-open, the sha-map sidecar, and
    // every walker/writer in the right order. We just surface the stats.
    let (stats, _map) = import_git_into_scoped_with_options(
        git_path,
        heddle_path,
        ImportOptions { lossy },
        ImportScope::refs(refs.to_vec()),
    )?;
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
