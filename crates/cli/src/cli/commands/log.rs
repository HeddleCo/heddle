// SPDX-License-Identifier: Apache-2.0
//! Log command.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use objects::object::{Agent, ChangeId, State};
use repo::{ChangedPathFilters, HistoryQuery, Repository, format_confidence, is_synthetic_root};
use serde::Serialize;

use super::{
    action_line::{format_next_step_dim, print_next_step},
    git_overlay_health::{PlainGitVerificationProbe, build_plain_git_verification_probe},
    history_target::resolve_state_id,
    snapshot::ensure_current_state,
};
use crate::{
    cli::{Cli, should_output_json, style},
    config::UserConfig,
};

#[derive(Clone, Debug)]
pub struct LogCommandOptions {
    pub state: Option<String>,
    pub limit: usize,
    pub all: bool,
    pub graph: bool,
    pub oneline: bool,
    pub reflog: bool,
    pub agent: Option<String>,
    pub paths: Vec<String>,
    /// Lower bound for the walk. Resolved through
    /// `resolve_state_id` so it accepts marker names, short
    /// state IDs, or any other spec the state resolver supports. Walk
    /// stops as soon as we encounter this state — the bound itself is
    /// excluded from the output (matches `git log --since`'s
    /// half-open semantics for time bounds).
    pub since: Option<String>,
}

#[derive(Serialize)]
struct LogOutput {
    output_kind: &'static str,
    status: &'static str,
    repository_capability: String,
    storage_model: String,
    states: Vec<StateEntry>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle bridge git status --output json` instead.
    #[serde(skip)]
    git_overlay_import_hint: Option<LogGitOverlayImportHintOutput>,
}

#[derive(Serialize)]
struct LogGitOverlayImportHintOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

#[derive(Serialize)]
struct StateEntry {
    change_id: String,
    content_hash: String,
    intent: Option<String>,
    principal: String,
    /// Raw principal name + email so we can render a styled
    /// `name <email>` pair (bold/dim) without re-parsing the
    /// pre-formatted `principal` string. Skipped from JSON
    /// serialization to keep the wire format unchanged — only the
    /// human-readable renderer reads them.
    #[serde(skip)]
    principal_name: String,
    #[serde(skip)]
    principal_email: String,
    agent: Option<String>,
    confidence: Option<f32>,
    created_at: String,
    parents: Vec<String>,
    git_checkpoint: Option<String>,
}

#[derive(Serialize)]
struct ReflogOutput {
    output_kind: &'static str,
    status: &'static str,
    repository_capability: String,
    storage_model: String,
    entries: Vec<ReflogEntry>,
}

#[derive(Clone, Debug, Serialize)]
struct ReflogEntry {
    source: String,
    reference: String,
    old_oid: String,
    new_oid: String,
    actor: String,
    timestamp: Option<String>,
    message: String,
}

impl From<&State> for StateEntry {
    fn from(state: &State) -> Self {
        Self {
            change_id: state.change_id.short(),
            content_hash: state.compute_hash().short(),
            intent: state.intent.clone(),
            principal: state.attribution.principal.to_string(),
            principal_name: state.attribution.principal.name.clone(),
            principal_email: state.attribution.principal.email.clone(),
            agent: state.attribution.agent.as_ref().map(Agent::to_string),
            confidence: state.confidence,
            created_at: state.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            parents: state.parents.iter().map(ChangeId::short).collect(),
            git_checkpoint: None,
        }
    }
}

pub async fn cmd_log(cli: &Cli, options: LogCommandOptions) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    if !options.reflog
        && options.state.is_none()
        && options.since.is_none()
        && options.paths.is_empty()
        && let Some(probe) = build_plain_git_verification_probe(start)?
    {
        return render_plain_git_log(cli, &probe, options.oneline);
    }

    let repo = Repository::open(start)?;

    if options.reflog {
        return cmd_log_reflog(cli, &repo, options.limit, options.oneline);
    }

    // Get starting state
    let start_id = if let Some(ref spec) = options.state {
        if matches!(spec.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
            ensure_current_state(
                &repo,
                &UserConfig::load_default().unwrap_or_default(),
                Some("Bootstrap git-overlay before viewing log".to_string()),
            )?;
        }
        Some(resolve_state_id(&repo, spec)?)
    } else {
        Some(ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before viewing log".to_string()),
        )?)
    };

    // Resolve the `--since` bound (if any) and pass it down as the
    // walker's `stop_at`. The bound is honored *before* `--agent` /
    // `--path` filters, so a bound state that itself doesn't match the
    // active filter still terminates the walk correctly. (Previously
    // we applied the bound *after* filtering, which silently leaked
    // matches older than the bound when the bound state was filtered
    // out.) The bound is exclusive — matches git's `--since` shape.
    let since_id = if let Some(ref spec) = options.since {
        Some(resolve_state_id(&repo, spec)?)
    } else {
        None
    };

    let changed_paths = ChangedPathFilters::try_from_paths(options.paths)?;
    let query = HistoryQuery::new(start_id)
        .with_limit(options.limit)
        .with_agent_filter(options.agent)
        .with_changed_paths(changed_paths)
        .with_stop_at(since_id);
    let states = repo.query_history(&query)?;

    // Hide the synthetic genesis root (`heddle init` seed). It carries
    // a stable system attribution rather than the user's principal —
    // see `repo::seed_default_thread`. Surfacing it in user-facing log
    // output would always show the seed state above the user's first
    // real snapshot and report a "Heddle <init@heddle>" or (in older
    // repos) "Unknown" principal that the user never authored.
    let output = LogOutput {
        output_kind: "log",
        status: "completed",
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        git_overlay_import_hint: repo.git_overlay_import_hint()?.map(|hint| {
            LogGitOverlayImportHintOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }
        }),
        states: states
            .iter()
            .filter(|state| !is_synthetic_root(state))
            .map(|state| {
                let mut entry = StateEntry::from(state);
                entry.git_checkpoint = repo
                    .latest_git_checkpoint_for_change(&state.change_id)
                    .ok()
                    .flatten()
                    .map(|record| record.git_commit);
                entry
            })
            .collect(),
    };

    let as_json = should_output_json(cli, Some(repo.config()));
    if as_json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        // Render to stdout via the writer-based helpers so the same
        // formatting logic is unit-testable against a `Vec<u8>` —
        // see `tests::print_oneline_no_ansi_when_disabled`.
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if options.oneline {
            let _ = write_oneline(&mut handle, &output, cli.verbose > 0);
        } else {
            let _ = write_full(&mut handle, &output, cli.verbose > 0);
        }
    }

    // Discoverability tip after a successful log: nudge toward
    // `heddle query` for filtered history. Once per session per repo.
    crate::cli::tips::maybe_emit(
        repo.root(),
        Some(repo.config()),
        crate::cli::tips::Tip::QueryFromLog,
        as_json,
        cli.quiet,
    );

    Ok(())
}

fn render_plain_git_log(cli: &Cli, probe: &PlainGitVerificationProbe, oneline: bool) -> Result<()> {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "output_kind": "log",
                "status": "blocked",
                "repository_capability": "plain-git",
                "storage_model": "git",
                "states": [],
                "verification": &probe.trust,
                "recommended_action": &probe.trust.recommended_action,
                "recovery_commands": &probe.trust.recovery_commands,
            }))?
        );
    } else if oneline {
        println!("plain-git Heddle not initialized; next: heddle init");
    } else {
        println!("Git repo, Heddle not initialized");
        if let Some(branch) = &probe.git_branch {
            println!("Git branch: {}", style::bold(branch));
        }
        println!("History: unavailable until this Git repo is initialized and imported");
        print_next_step("heddle init");
        if let Some(branch) = &probe.git_branch {
            println!(
                "Then: {}",
                style::bold(&super::git_overlay_health::canonical_adopt_ref_command(
                    branch
                ))
            );
        }
    }
    Ok(())
}

fn cmd_log_reflog(cli: &Cli, repo: &Repository, limit: usize, oneline: bool) -> Result<()> {
    let output = ReflogOutput {
        output_kind: "log_reflog",
        status: "completed",
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        entries: collect_reflog_entries(repo.root(), limit)?,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if oneline {
            let _ = write_reflog_oneline(&mut handle, &output);
        } else {
            let _ = write_reflog_full(&mut handle, &output);
        }
    }

    Ok(())
}

fn collect_reflog_entries(root: &Path, limit: usize) -> Result<Vec<ReflogEntry>> {
    let mut entries = Vec::new();
    for (source, logs_dir) in reflog_roots(root)? {
        collect_reflog_dir(&source, &logs_dir, &mut entries)
            .with_context(|| format!("reading reflog entries from {}", logs_dir.display()))?;
    }

    entries.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| a.reference.cmp(&b.reference))
            .then_with(|| a.message.cmp(&b.message))
    });
    entries.truncate(limit);
    Ok(entries)
}

fn reflog_roots(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut roots = Vec::new();

    if let Some(git_dir) = checkout_git_dir(root)? {
        let logs = git_dir.join("logs");
        if logs.is_dir() {
            roots.push(("checkout".to_string(), logs));
        }
    }

    let mirror_logs = root.join(".heddle").join("git").join("logs");
    if mirror_logs.is_dir() {
        roots.push(("mirror".to_string(), mirror_logs));
    }

    Ok(roots)
}

fn checkout_git_dir(root: &Path) -> Result<Option<PathBuf>> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    if !dot_git.is_file() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&dot_git)
        .with_context(|| format!("reading gitdir pointer {}", dot_git.display()))?;
    let Some(path) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let path = PathBuf::from(path.trim());
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Ok(Some(root.join(path)))
    }
}

fn collect_reflog_dir(source: &str, logs_dir: &Path, entries: &mut Vec<ReflogEntry>) -> Result<()> {
    let mut stack = vec![logs_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(reference) = path.strip_prefix(logs_dir) else {
                continue;
            };
            let reference = reference
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            read_reflog_file(source, &reference, &path, entries)?;
        }
    }
    Ok(())
}

fn read_reflog_file(
    source: &str,
    reference: &str,
    path: &Path,
    entries: &mut Vec<ReflogEntry>,
) -> Result<()> {
    let file = fs::File::open(path)?;
    for line in io::BufReader::new(file).lines() {
        if let Some(entry) = parse_reflog_line(source, reference, &line?) {
            entries.push(entry);
        }
    }
    Ok(())
}

fn parse_reflog_line(source: &str, reference: &str, line: &str) -> Option<ReflogEntry> {
    let (metadata, message) = line.split_once('\t').unwrap_or((line, ""));
    let mut parts = metadata.split_whitespace();
    let old_oid = parts.next()?.to_string();
    let new_oid = parts.next()?.to_string();
    let mut actor_parts = Vec::new();
    let mut timestamp = None;

    for part in parts {
        if part.parse::<i64>().is_ok() {
            timestamp = Some(part.to_string());
            break;
        }
        actor_parts.push(part);
    }

    Some(ReflogEntry {
        source: source.to_string(),
        reference: reference.to_string(),
        old_oid,
        new_oid,
        actor: actor_parts.join(" "),
        timestamp,
        message: message.to_string(),
    })
}

fn write_reflog_oneline<W: std::io::Write>(
    out: &mut W,
    output: &ReflogOutput,
) -> std::io::Result<()> {
    for entry in &output.entries {
        writeln!(
            out,
            "{} {} {} {}",
            style::dim(&entry.source),
            style::change_id(short_oid(&entry.new_oid)),
            style::dim(&entry.reference),
            style::bold(&entry.message)
        )?;
    }
    Ok(())
}

fn write_reflog_full<W: std::io::Write>(out: &mut W, output: &ReflogOutput) -> std::io::Result<()> {
    writeln!(
        out,
        "Repository: {}",
        crate::cli::render::repository_mode_label(
            &output.repository_capability,
            &output.storage_model
        )
    )?;
    writeln!(out, "Reflog: {} entrie(s)", output.entries.len())?;
    if output.entries.is_empty() {
        if let Some(line) = format_next_step_dim(
            "make a checkpoint, fetch, pull, push, or run `heddle adopt`",
            0,
        ) {
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    let mut by_ref: BTreeMap<(&str, &str), Vec<&ReflogEntry>> = BTreeMap::new();
    for entry in &output.entries {
        by_ref
            .entry((&entry.source, &entry.reference))
            .or_default()
            .push(entry);
    }

    for ((source, reference), entries) in by_ref {
        writeln!(out)?;
        writeln!(
            out,
            "{} {}",
            style::bold(reference),
            style::dim(&format!("({source})"))
        )?;
        for entry in entries {
            writeln!(
                out,
                "  {} {} -> {} {}",
                style::dim(entry.timestamp.as_deref().unwrap_or("unknown-time")),
                style::dim(short_oid(&entry.old_oid)),
                style::accent(short_oid(&entry.new_oid)),
                style::bold(&entry.message)
            )?;
            if !entry.actor.is_empty() {
                writeln!(out, "    by {}", style::dim(&entry.actor))?;
            }
        }
    }
    Ok(())
}

fn short_oid(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

fn write_oneline<W: std::io::Write>(
    out: &mut W,
    output: &LogOutput,
    verbose: bool,
) -> std::io::Result<()> {
    for entry in &output.states {
        let intent = entry.intent.as_deref().unwrap_or("(no intent)");
        let checkpoint = if verbose && entry.git_checkpoint.is_some() {
            " [git]"
        } else {
            ""
        };
        if verbose {
            // Three columns of decreasing emphasis: id (dim,
            // structural), hash (dim, structural), intent (bold, the
            // part you read). Default text hides the content hash
            // because the stable change id is the user-facing anchor.
            writeln!(
                out,
                "{} {} {}{}",
                style::change_id(&entry.change_id),
                style::dim(&entry.content_hash),
                style::bold(intent),
                checkpoint,
            )?;
        } else {
            writeln!(
                out,
                "{} {}",
                style::change_id(&entry.change_id),
                style::bold(intent),
            )?;
        }
    }
    Ok(())
}

fn write_full<W: std::io::Write>(
    out: &mut W,
    output: &LogOutput,
    verbose: bool,
) -> std::io::Result<()> {
    // The mode preamble is diagnostic noise on the common read path
    // (heddle#275); `heddle status` already exposes it. Keep it under
    // `-v` for troubleshooting.
    if verbose {
        writeln!(
            out,
            "Repository: {}",
            crate::cli::render::repository_mode_label(
                &output.repository_capability,
                &output.storage_model
            )
        )?;
    }
    if let Some(hint) = &output.git_overlay_import_hint {
        writeln!(
            out,
            "{}",
            crate::cli::render::git_only_branch_summary(
                &hint.missing_branches,
                hint.missing_branch_count,
            )
        )?;
        if let Some(line) = format_next_step_dim(&hint.recommended_command, 0) {
            writeln!(out, "{line}")?;
        }
    }
    writeln!(out)?;
    for (i, entry) in output.states.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }

        if verbose {
            // Header: change-id and tree hash dim, timestamp dim.
            // Nothing carries semantic color here — these are
            // structurally important but not the focus.
            writeln!(
                out,
                "{} ({}) {}",
                style::change_id(&entry.change_id),
                style::dim(&entry.content_hash),
                style::dim(&entry.created_at),
            )?;
        } else {
            writeln!(
                out,
                "{} {}",
                style::change_id(&entry.change_id),
                style::dim(&entry.created_at),
            )?;
        }

        if let Some(intent) = &entry.intent {
            // Intent is the editorial line — bold, no color.
            writeln!(out, "  {}", style::bold(intent))?;
        }

        if verbose {
            writeln!(
                out,
                "  Principal: {}",
                style::principal(&entry.principal_name, &entry.principal_email)
            )?;
        }

        if verbose && let Some(agent) = &entry.agent {
            writeln!(out, "  Agent: {}", style::dim(agent))?;
        }

        // Confidence only renders when set — the per-entry "Confidence: —"
        // sentinel was high-noise on long logs and conveyed "value absent"
        // information the absence-of-line itself already encodes. JSON output
        // still carries `confidence: null` so agents can distinguish.
        if entry.confidence.is_some() {
            let confidence_text = format_confidence(entry.confidence);
            writeln!(
                out,
                "  Confidence: {}",
                style::confidence(entry.confidence, &confidence_text)
            )?;
        }
        // Durability only renders when there's something to say — a Git
        // checkpoint binds the capture into Git history. The "Capture
        // durability: local only" fallback used to repeat on every entry and
        // told the user nothing they couldn't infer from the absence of a
        // Git-checkpoint line.
        if verbose && let Some(git_checkpoint) = &entry.git_checkpoint {
            writeln!(
                out,
                "  Git checkpoint: {}",
                style::dim(&git_checkpoint[..std::cmp::min(12, git_checkpoint.len())])
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    fn sample_entry() -> StateEntry {
        StateEntry {
            change_id: "hd-abc123".to_string(),
            content_hash: "deadbeef".to_string(),
            intent: Some("Capture audit pipeline".to_string()),
            principal: "Ada <ada@example.com>".to_string(),
            principal_name: "Ada".to_string(),
            principal_email: "ada@example.com".to_string(),
            agent: Some("anthropic/claude-opus-4".to_string()),
            confidence: Some(0.95),
            created_at: "2026-05-01 12:00:00".to_string(),
            parents: vec![],
            git_checkpoint: Some("abc123def456".to_string()),
        }
    }

    /// Render-site smoke test: with color disabled, neither the
    /// oneline nor the full renderer must emit any ANSI escape.
    /// This is the integration check that `style::*` helpers
    /// short-circuit to plain text — if a future helper forgets
    /// the gate, this test catches the leak before it reaches a
    /// user pipe or test snapshot.
    #[test]
    #[serial(color_state)]
    fn render_sites_no_ansi_when_disabled() {
        style::force_for_test(false);
        let output = LogOutput {
            output_kind: "log",
            status: "completed",
            repository_capability: "git-overlay".to_string(),
            storage_model: "git+heddle-sidecar".to_string(),
            git_overlay_import_hint: None,
            states: vec![sample_entry()],
        };

        let mut buf = Vec::new();
        write_oneline(&mut buf, &output, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'), "oneline leaked ANSI: {s:?}");
        assert!(s.contains("hd-abc123"));
        assert!(s.contains("Capture audit pipeline"));

        let mut buf = Vec::new();
        write_full(&mut buf, &output, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'), "full leaked ANSI: {s:?}");
        assert!(!s.contains("Ada <ada@example.com>"));
        assert!(s.contains("Confidence: 0.95"));

        let mut buf = Vec::new();
        write_full(&mut buf, &output, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Ada <ada@example.com>"));
    }

    /// With color enabled, the same renderer must emit escapes —
    /// otherwise the gate is stuck off and we'd silently ship a
    /// monochrome CLI. This pairs with the negative test above to
    /// pin both directions of the gate.
    #[test]
    #[serial(color_state)]
    fn render_sites_emit_ansi_when_enabled() {
        style::force_for_test(true);
        let output = LogOutput {
            output_kind: "log",
            status: "completed",
            repository_capability: "git-overlay".to_string(),
            storage_model: "git+heddle-sidecar".to_string(),
            git_overlay_import_hint: None,
            states: vec![sample_entry()],
        };
        let mut buf = Vec::new();
        write_oneline(&mut buf, &output, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('\x1b'), "expected ANSI in oneline: {s:?}");
    }

    /// The default full text view must lead with log data, not the
    /// `Repository:` mode preamble — that line is noise on every read
    /// (heddle#275). `-v` keeps it for diagnostic context.
    #[test]
    #[serial(color_state)]
    fn write_full_gates_repository_preamble_on_verbose() {
        style::force_for_test(false);
        let output = LogOutput {
            output_kind: "log",
            status: "completed",
            repository_capability: "git-overlay".to_string(),
            storage_model: "git+heddle-sidecar".to_string(),
            git_overlay_import_hint: None,
            states: vec![sample_entry()],
        };

        let mut buf = Vec::new();
        write_full(&mut buf, &output, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains("Repository:"),
            "default full log leaked the mode preamble: {s:?}"
        );

        let mut buf = Vec::new();
        write_full(&mut buf, &output, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("Repository:"),
            "verbose full log should retain the mode preamble: {s:?}"
        );
    }
}
