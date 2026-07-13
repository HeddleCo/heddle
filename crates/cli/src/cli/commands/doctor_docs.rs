// SPDX-License-Identifier: Apache-2.0
//! `heddle doctor docs` — diff-check markdown docs against the live CLI surface.
//!
//! Walks every `heddle <verb> [<subverb>] [flags]` invocation embedded in a
//! markdown file and reports any drift: verbs that no longer exist, long
//! flags that aren't on that verb, or literal values for finite-valued
//! flags that aren't in the valid set. Current guidance also receives a
//! semantic policy pass for retired and authority-inconsistent commands.
//!
//! The check is built on top of clap's own `Cli::command()`, so it's
//! always in sync with the binary you're running. Wire `heddle doctor
//! docs --all --output json` into CI on every PR to catch doc drift.
//!
//! Pure invocation extraction/tokenization lives in
//! `heddle_core::doctor_docs_plan`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use clap::{Command as ClapCommand, CommandFactory};
use heddle_core::doctor_docs_plan::{
    DocsInvocation, display_path, extract_invocations, looks_like_value,
};
use objects::worktree::should_ignore;
use serde::Serialize;
use serde_json::{Map, Value};

use super::{
    RecoveryAdvice,
    command_catalog::{
        ActionTemplate, CommandCatalogOption, CommandCatalogOutput, build_command_catalog,
        feature_gated_command_roots, recommended_action_template,
    },
};
use crate::cli::{Cli, DoctorDocsArgs, should_output_json};

/// One drift finding.
#[derive(Debug, Clone, Serialize)]
pub struct DocsIssue {
    pub file: String,
    pub line: usize,
    pub invocation: String,
    pub kind: IssueKind,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Categories of drift the checker can detect.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    /// The top-level verb (e.g. `heddle foo`) is not in the CLI.
    UnknownVerb,
    /// The subverb (e.g. `heddle thread marker fizz`) is not under that verb.
    UnknownSubverb,
    /// A long flag (`--xyz`) is not defined on the resolved (sub)verb.
    UnknownFlag,
    /// A literal value passed to `--workspace`/`--scope`/`--kind` is not
    /// in the valid set for that flag.
    InvalidFlagValue,
    /// A syntactically valid or historical command has been removed from the
    /// supported product vocabulary.
    RetiredCommand,
    /// The recommendation sends a source operation through Heddle when durable
    /// repository authority requires direct Git or a separate workflow step.
    AuthorityConflict,
    /// The file referenced by `--path` (or enumerated by `--all`) could
    /// not be read. We surface this as a real issue, not a silent skip,
    /// so a typoed path or permission error fails CI instead of letting
    /// a "scanned 0 files" result pass.
    Unreadable,
}

/// Aggregate output for the verb (human and JSON share this shape).
#[derive(Debug, Clone, Serialize)]
pub struct DocsReport {
    pub output_kind: &'static str,
    pub status: &'static str,
    #[serde(rename = "verified")]
    pub verified: bool,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    pub files_scanned: usize,
    pub issues: Vec<DocsIssue>,
}

/// Public entrypoint wired from `main.rs`.
pub fn cmd_doctor_docs(cli: &Cli, args: DoctorDocsArgs) -> Result<()> {
    let json = should_output_json(cli, None);
    let repo_root = cli.repo.clone().map(Ok).unwrap_or_else(|| {
        std::env::current_dir().map(|cwd| find_repo_root(&cwd).unwrap_or(cwd))
    })?;

    let files = resolve_files(&repo_root, &args)?;
    let cli_command = Cli::command();
    let mut issues = Vec::new();
    for file in &files {
        let display = display_path(&repo_root, file);
        let bytes = match std::fs::read_to_string(file) {
            Ok(b) => b,
            Err(err) => {
                // Unreadable files used to be a silent skip + stderr
                // log, which let typoed `--path` arguments and missing
                // files pass CI with a "scanned 0 files" result. Treat
                // them as real drift findings: the issue list is what
                // gates the non-zero exit below.
                issues.push(DocsIssue {
                    file: display,
                    line: 0,
                    invocation: String::new(),
                    kind: IssueKind::Unreadable,
                    detail: format!("could not read {}: {}", file.display(), err),
                    suggestion: None,
                });
                continue;
            }
        };
        scan_markdown(&display, &bytes, &cli_command, &mut issues);
    }

    let clean = issues.is_empty();
    let recommended_action = (!clean).then(|| "heddle doctor docs --all --output json".to_string());
    let recommended_action_template = recommended_action
        .as_deref()
        .and_then(recommended_action_template);
    let report = DocsReport {
        output_kind: "doctor_docs",
        status: if clean { "clean" } else { "drift" },
        verified: clean,
        recommended_action,
        recommended_action_template,
        files_scanned: files.len(),
        issues,
    };

    if !report.issues.is_empty() {
        if json {
            return Err(anyhow!(doctor_docs_drift_advice(&report)?));
        }
        render_human(&report);
        return Err(anyhow!(
            "{} drift issue(s) found across {} file(s)",
            report.issues.len(),
            report.files_scanned
        ));
    }

    if json {
        let s = serde_json::to_string_pretty(&report)?;
        println!("{s}");
    } else {
        render_human(&report);
    }
    Ok(())
}

fn doctor_docs_drift_advice(report: &DocsReport) -> Result<RecoveryAdvice> {
    let primary = report
        .recommended_action
        .clone()
        .unwrap_or_else(|| "heddle doctor docs --all --output json".to_string());
    let mut advice = RecoveryAdvice::safety_refusal(
        "machine_contract_drift",
        format!(
            "{} docs drift issue(s) found across {} file(s)",
            report.issues.len(),
            report.files_scanned
        ),
        format!(
            "Inspect the issue list in this envelope, then run `{primary}` after updating docs or command metadata."
        ),
        format!(
            "documented Heddle invocations no longer match the registered CLI surface: {} issue(s)",
            report.issues.len()
        ),
        "agents could follow stale commands, flags, or values from the public documentation",
        "repository state, refs, metadata, and worktree files were left unchanged",
        primary.clone(),
        vec![primary],
    );
    let mut extra = Map::new();
    extra.insert(
        "output_kind".to_string(),
        Value::String(report.output_kind.to_string()),
    );
    extra.insert(
        "status".to_string(),
        Value::String(report.status.to_string()),
    );
    extra.insert("verified".to_string(), Value::Bool(report.verified));
    extra.insert(
        "recommended_action".to_string(),
        serde_json::to_value(&report.recommended_action)?,
    );
    extra.insert(
        "recommended_action_template".to_string(),
        serde_json::to_value(&report.recommended_action_template)?,
    );
    extra.insert(
        "files_scanned".to_string(),
        serde_json::json!(report.files_scanned),
    );
    extra.insert("issues".to_string(), serde_json::to_value(&report.issues)?);
    advice.extra_json_fields = extra;
    Ok(advice)
}

fn render_human(report: &DocsReport) {
    if report.issues.is_empty() {
        println!(
            "doctor docs: no drift found across {} file(s)",
            report.files_scanned
        );
        return;
    }
    println!(
        "doctor docs: {} drift issue(s) across {} file(s)",
        report.issues.len(),
        report.files_scanned
    );
    println!();
    for issue in &report.issues {
        println!("{}:{}", issue.file, issue.line);
        println!("  {}", issue.invocation);
        println!("  {:?}: {}", issue.kind, issue.detail);
        if let Some(suggestion) = &issue.suggestion {
            println!("  suggestion: {}", suggestion);
        }
        println!();
    }
}

/// Resolve which files to scan. `--path` wins; otherwise (or with
/// `--all`) we walk the repo root natively and pick up every `.md`
/// that isn't under a heddle-ignored prefix. Native-heddle repos
/// have no `.git/`, so shelling out to `git ls-files` (the previous
/// implementation) hard-failed there. The native walk also lets the
/// command run in environments without git installed.
fn resolve_files(repo_root: &Path, args: &DoctorDocsArgs) -> Result<Vec<PathBuf>> {
    if !args.path.is_empty() && !args.all {
        return Ok(args
            .path
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    repo_root.join(p)
                }
            })
            .collect());
    }
    let mut out = Vec::new();
    walk_markdown(repo_root, repo_root, &mut out)?;
    out.sort();
    Ok(out)
}

/// Common directories the markdown enumerator should never descend
/// into. Mirrored from `objects::worktree::should_ignore` patterns
/// the broader codebase already uses; kept inline so this verb works
/// even in repos that don't ship a `.heddleignore`. Order matches the
/// surface most likely to bury thousands of irrelevant `.md` files
/// (build outputs, deps).
const IGNORE_PATTERNS: &[&str] = &[
    ".git",
    ".codex/",
    ".heddle",
    ".heddleignore",
    "target/",
    "node_modules/",
    "dist/",
    "build/",
    ".venv/",
    "venv/",
    ".tox/",
    ".cache/",
    ".idea/",
    ".vscode/",
    // Spike/design docs deliberately reference planned-but-unbuilt or retired
    // verbs and flags to argue for a shape. Drift-checking would force every
    // author to either annotate each example or hold the spike to the current
    // CLI surface. Exempt the directory wholesale.
    "docs/spikes/",
];

/// Markdown files larger than this are skipped — almost always
/// vendored docs (LICENSEs in markdown, generated API references)
/// where drift checking would burn time without finding real CLI
/// invocations. Matches the rough cutoff used by other heddle
/// scanners. 1 MiB is generous for hand-authored prose.
const MAX_MARKDOWN_BYTES: u64 = 1024 * 1024;

/// Recursive walk rooted at `dir`. `repo_root` is fixed across the
/// recursion so we can produce repo-relative paths for the ignore
/// matcher. We push absolute paths into `out`; sorting happens at
/// the call site.
fn walk_markdown(repo_root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A directory we can't read isn't fatal here — `--all` is a
        // best-effort enumeration. Surface as a warning and skip; the
        // unreadable-file path covers the case where a user-supplied
        // `--path` is missing.
        Err(err) => {
            tracing::warn!(
                dir = %dir.display(),
                %err,
                "doctor docs: skipping unreadable directory during --all walk"
            );
            return Ok(());
        }
    };
    let ignore_owned: Vec<String> = IGNORE_PATTERNS.iter().map(|s| (*s).to_string()).collect();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(repo_root) else {
            continue;
        };
        if should_ignore(rel, &ignore_owned) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk_markdown(repo_root, &path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        // Skip oversize markdown — see `MAX_MARKDOWN_BYTES`.
        if let Ok(meta) = entry.metadata()
            && meta.len() > MAX_MARKDOWN_BYTES
        {
            continue;
        }
        out.push(path);
    }
    Ok(())
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() || current.join(".heddle").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Walk a markdown buffer and emit drift issues into `out`.
pub fn scan_markdown(
    display_path: &str,
    text: &str,
    cli_command: &ClapCommand,
    out: &mut Vec<DocsIssue>,
) {
    let catalog = build_command_catalog();
    let historical = is_historical_design_record(display_path);
    if !historical {
        scan_forbidden_prose(display_path, text, out);
    }
    let invocations = extract_invocations(text);
    for inv in invocations {
        if let Some(issue) = forbidden_invocation_issue(display_path, &inv) {
            if !historical {
                out.push(issue);
            }
            continue;
        }
        check_invocation(display_path, &inv, cli_command, &catalog, out);
    }
}

fn is_historical_design_record(display_path: &str) -> bool {
    let normalized = display_path.replace('\\', "/");
    normalized.starts_with("docs/adr/") || normalized.starts_with("docs/spikes/")
}

fn scan_forbidden_prose(file: &str, text: &str, out: &mut Vec<DocsIssue>) {
    let mut in_fence = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if line.contains('`') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let machine_recommendation = lower.contains("recommended_action")
            || lower.contains("next_action")
            || lower.contains("primary_command");
        let prose_recommendation = ["run heddle ", "use heddle ", "invoke heddle "]
            .iter()
            .any(|cue| lower.contains(cue));
        if (in_fence && !machine_recommendation)
            || (!in_fence && !machine_recommendation && !prose_recommendation)
        {
            continue;
        }
        let verb = [
            "checkpoint",
            "cherry-pick",
            "git-overlay",
            "support",
            "spool",
            "prove",
            "presence",
            "actor",
            "session",
            "switch",
            "stash",
            "clean",
            "fetch",
            "merge",
            "rebase",
        ]
        .into_iter()
        .find(|verb| contains_command_phrase(&lower, verb));
        let land_publishes = contains_command_phrase(&lower, "land")
            && ["--push", "--no-push", "--publish", "--no-publish"]
                .iter()
                .any(|flag| lower.contains(flag));
        let Some(verb) = verb.or(land_publishes.then_some("land")) else {
            continue;
        };
        let tokens = if verb == "land" {
            vec!["land".to_string(), "--push".to_string()]
        } else {
            vec![verb.to_string()]
        };
        let invocation = DocsInvocation {
            line: index + 1,
            raw: line.trim().to_string(),
            tokens,
        };
        if let Some(issue) = forbidden_invocation_issue(file, &invocation) {
            out.push(issue);
        }
    }
}

fn contains_command_phrase(line: &str, verb: &str) -> bool {
    let phrase = format!("heddle {verb}");
    line.match_indices(&phrase).any(|(index, _)| {
        line[index + phrase.len()..]
            .chars()
            .next()
            .is_none_or(|next| !next.is_ascii_alphanumeric() && next != '-' && next != '_')
    })
}

fn forbidden_invocation_issue(file: &str, inv: &DocsInvocation) -> Option<DocsIssue> {
    let verb = inv.tokens.first()?.as_str();
    let retired = matches!(
        verb,
        "checkpoint"
            | "cherry-pick"
            | "git-overlay"
            | "support"
            | "spool"
            | "prove"
            | "presence"
            | "actor"
            | "session"
    );
    if retired {
        let suggestion = match verb {
            "checkpoint" => "use `heddle capture`, then `heddle commit` in Git Overlay",
            "actor" | "session" => "inspect the `heddle agent` command family",
            _ => "remove the retired Heddle command recommendation",
        };
        return Some(DocsIssue {
            file: file.to_string(),
            line: inv.line,
            invocation: inv.raw.clone(),
            kind: IssueKind::RetiredCommand,
            detail: format!("`heddle {verb}` is outside the contracted Heddle CLI"),
            suggestion: Some(suggestion.to_string()),
        });
    }

    if matches!(
        verb,
        "switch" | "stash" | "clean" | "fetch" | "merge" | "rebase"
    ) {
        return Some(DocsIssue {
            file: file.to_string(),
            line: inv.line,
            invocation: inv.raw.clone(),
            kind: IssueKind::AuthorityConflict,
            detail: format!(
                "`heddle {verb}` is outside Heddle's narrow Git surface; use a compatible client if that operation is required"
            ),
            suggestion: Some(format!("use `git {verb}` in Git Overlay documentation")),
        });
    }

    if verb == "land"
        && inv.tokens.iter().skip(1).any(|token| {
            matches!(
                token.as_str(),
                "--push" | "--no-push" | "--publish" | "--no-publish"
            )
        })
    {
        return Some(DocsIssue {
            file: file.to_string(),
            line: inv.line,
            invocation: inv.raw.clone(),
            kind: IssueKind::AuthorityConflict,
            detail: "`heddle land` is local integration and has no publication flags".to_string(),
            suggestion: Some(
                "land locally, then publish with the source-authority command".to_string(),
            ),
        });
    }

    None
}

fn check_invocation(
    file: &str,
    inv: &DocsInvocation,
    cli_command: &ClapCommand,
    catalog: &CommandCatalogOutput,
    out: &mut Vec<DocsIssue>,
) {
    if inv.tokens.is_empty() {
        return;
    }
    let verb = inv.tokens[0].as_str();
    // Skip placeholders — `<command>`, `[OPTIONS]`, bare ellipses,
    // and lone flags can't be resolved as verbs. Also skip tokens
    // ending in `:` (shell prompt labels, e.g. `$ heddle <state>:`)
    // and tokens with internal colons (rendered prompts).
    if verb.starts_with('<')
        || verb.starts_with('[')
        || verb.starts_with('{')
        || verb.starts_with('-')
        || verb.ends_with(':')
        || verb == "..."
        || verb.is_empty()
    {
        return;
    }

    // Verbs that exist in the source but are gated behind a non-default
    // Cargo feature aren't visible on `Cli::command()` here. Don't
    // false-positive on docs that describe them — agents and humans
    // both reach for these surfaces in real builds.
    if feature_gated_command_roots().contains(&verb) {
        return;
    }

    // Resolve verb (and optional subverb) against the clap tree.
    let Some(verb_cmd) = find_subcommand(cli_command, verb) else {
        out.push(DocsIssue {
            file: file.to_string(),
            line: inv.line,
            invocation: inv.raw.clone(),
            kind: IssueKind::UnknownVerb,
            detail: format!("`heddle {}` is not a known verb", verb),
            suggestion: suggest_known_alt(cli_command, verb),
        });
        return;
    };

    // Greedily descend through nested subcommands while the next token
    // looks like an identifier (not a flag, placeholder, or path). We
    // stop at the first non-identifier or when the current command has
    // no more subcommands. This handles cases like `heddle context reason
    // git --path <…>` where the leaf is two levels below the
    // top-level verb.
    let mut resolved_cmd = verb_cmd;
    let mut tokens_used = 1;
    let mut path_segments = vec![verb_cmd.get_name().to_string()];
    while tokens_used < inv.tokens.len() {
        let next = &inv.tokens[tokens_used];
        if next.starts_with('-')
            || next.starts_with('<')
            || next.starts_with('[')
            || next.contains('/')
            || looks_like_value(next)
        {
            break;
        }
        if let Some(sub) = find_subcommand(resolved_cmd, next) {
            resolved_cmd = sub;
            path_segments.push(sub.get_name().to_string());
            tokens_used += 1;
            continue;
        }
        // No match. If the current command has further subcommands,
        // this is an unknown subverb — flag it. Otherwise (the current
        // command is a leaf with positional args), just stop and let
        // flag-checking proceed.
        if resolved_cmd.get_subcommands().next().is_some() {
            out.push(DocsIssue {
                file: file.to_string(),
                line: inv.line,
                invocation: inv.raw.clone(),
                kind: IssueKind::UnknownSubverb,
                detail: format!(
                    "`heddle {} {}` is not a known subcommand of `{}`",
                    path_segments.join(" "),
                    next,
                    path_segments.join(" "),
                ),
                suggestion: suggest_known_alt(resolved_cmd, next),
            });
            return;
        }
        break;
    }

    // Now walk remaining tokens for `--flag` / `--flag=value` shapes.
    let mut i = tokens_used;
    let catalog_options = collect_catalog_options(catalog, &path_segments);
    // The everyday catalog drops `hide = true` args, but they are still part
    // of the registered CLI contract (e.g. `capture --help-agent`). Recognize
    // them so docs that reference a hidden-but-real flag don't false-positive
    // as drift. Closes the class for every hidden flag, not just one.
    let hidden_long_flags = collect_hidden_long_flags(resolved_cmd);
    while i < inv.tokens.len() {
        let tok = &inv.tokens[i];
        if let Some(flag_body) = tok.strip_prefix("--") {
            // Skip empty `--` (POSIX end-of-options) and pure placeholders.
            if flag_body.is_empty() || flag_body.starts_with('<') {
                i += 1;
                continue;
            }
            let (flag_name, inline_value) = match flag_body.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (flag_body.to_string(), None),
            };
            // Many docs use `--flag-name>` accidentally; guard.
            let flag_name = flag_name.trim_end_matches('>').to_string();
            if !catalog_options.contains_key(&flag_name) {
                if !hidden_long_flags.contains(&flag_name) {
                    out.push(DocsIssue {
                        file: file.to_string(),
                        line: inv.line,
                        invocation: inv.raw.clone(),
                        kind: IssueKind::UnknownFlag,
                        detail: format!(
                            "`--{}` is not a flag on `heddle {}`",
                            flag_name,
                            path_segments.join(" "),
                        ),
                        suggestion: None,
                    });
                }
            } else {
                // Check known-enum flags. Pull value from inline or
                // next token (if not a flag/placeholder).
                let value = match inline_value {
                    Some(v) => Some(v),
                    None => inv.tokens.get(i + 1).and_then(|next| {
                        if next.starts_with('-') || next.starts_with('<') {
                            None
                        } else {
                            Some(next.clone())
                        }
                    }),
                };
                if let Some(value) = value
                    && let Some((valid, sug)) =
                        validate_flag_value(catalog_options[&flag_name], &value)
                    && !valid
                {
                    out.push(DocsIssue {
                        file: file.to_string(),
                        line: inv.line,
                        invocation: inv.raw.clone(),
                        kind: IssueKind::InvalidFlagValue,
                        detail: format!("`--{} {}` is not in the valid set", flag_name, value),
                        suggestion: sug,
                    });
                }
            }
        }
        i += 1;
    }
}

fn find_subcommand<'a>(cmd: &'a ClapCommand, name: &str) -> Option<&'a ClapCommand> {
    cmd.get_subcommands().find(|sc| {
        sc.get_name() == name
            || sc.get_visible_aliases().any(|alias| alias == name)
            || sc.get_all_aliases().any(|alias| alias == name)
    })
}

fn collect_catalog_options<'a>(
    catalog: &'a CommandCatalogOutput,
    path_segments: &[String],
) -> BTreeMap<String, &'a CommandCatalogOption> {
    let Some(options) = catalog.options_for_path(path_segments) else {
        return BTreeMap::new();
    };
    options
        .into_iter()
        .flat_map(|option| {
            option
                .long
                .iter()
                .chain(option.aliases.iter())
                .map(move |name| (name.clone(), option))
        })
        .collect()
}

/// Long flag names (and aliases) of the `hide = true` args on a resolved
/// command. These are dropped from the everyday command catalog but remain
/// part of the registered CLI surface, so `doctor docs` must still treat
/// them as valid flags.
fn collect_hidden_long_flags(command: &ClapCommand) -> BTreeSet<String> {
    command
        .get_arguments()
        .filter(|arg| arg.is_hide_set())
        .flat_map(|arg| {
            arg.get_long()
                .map(str::to_string)
                .into_iter()
                .chain(
                    arg.get_all_aliases()
                        .unwrap_or_default()
                        .into_iter()
                        .map(str::to_string),
                )
                .chain(
                    arg.get_visible_aliases()
                        .unwrap_or_default()
                        .into_iter()
                        .map(str::to_string),
                )
        })
        .collect()
}

fn suggest_known_alt(parent: &ClapCommand, _wrong: &str) -> Option<String> {
    // Cheap "did you mean" surface: just list a couple of close hits.
    let names: Vec<&str> = parent.get_subcommands().map(|sc| sc.get_name()).collect();
    if names.is_empty() {
        return None;
    }
    let preview: Vec<&&str> = names.iter().take(6).collect();
    Some(format!(
        "known: {}",
        preview
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Returns Some((is_valid, suggestion_string)) if `option` advertises finite
/// values in the command catalog, else None to mean "unchecked".
fn validate_flag_value(
    option: &CommandCatalogOption,
    value: &str,
) -> Option<(bool, Option<String>)> {
    if option.possible_values.is_empty()
        || matches!(option.value_kind.as_str(), "boolean" | "count")
    {
        return None;
    }
    // Strip placeholder shapes (`<name>`, `"…"`, etc.) — we only validate
    // literal values.
    if value.starts_with('<') || value.starts_with('"') || value.starts_with('\'') {
        return None;
    }
    let valid = option
        .possible_values
        .iter()
        .any(|candidate| candidate == value);
    let suggestion = if valid {
        None
    } else {
        option
            .long
            .as_ref()
            .map(|flag| format!("use --{flag} {{{}}}", option.possible_values.join("|")))
    };
    Some((valid, suggestion))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn cli() -> ClapCommand {
        Cli::command()
    }

    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    // Guard against stale references to folded/deleted verbs in
    // user-facing source strings (recovery breadcrumbs, advice, help
    // text, error messages, emitted doc comments). Phase-1 of the CLI
    // consolidation (heddle#473) folded `gc`/`monitor`/`checkout`/… into
    // their canonical parents, but free-text strings that still spelled
    // the old verb slipped past the markdown-only `doctor docs` check —
    // that scanner only reads `.md` files, not `.rs` sources. We reuse
    // the exact same backtick-invocation extractor + clap/catalog
    // resolution against every Rust source file so a future fold can't
    // leave an invalid `heddle <verb>` behind in an emitted string.
    //
    // Validity is data-driven off the live clap command tree +
    // feature-gated catalog roots — never a hardcoded list — so it stays
    // correct as verbs are added or removed.
    #[test]
    fn source_strings_reference_only_current_top_level_verbs() {
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let cli_command = cli();

        // The docs-drift checker's own surfaces intentionally embed
        // invalid `heddle foo`/`bar`/`frobnicate` examples (enum docs +
        // negative-test fixtures). Skipping them keeps the guard's
        // source-of-truth the *real* command set rather than forcing a
        // denylist of pedagogical tokens.
        const SELF_TEST_SURFACES: &[&str] = &["doctor_docs.rs", "doctor_schemas.rs"];

        let mut rs_files = Vec::new();
        collect_rs_files(&src_root, &mut rs_files);
        assert!(
            rs_files.len() > 50,
            "expected to walk the cli source tree; only found {} .rs files under {}",
            rs_files.len(),
            src_root.display(),
        );

        let mut stale = Vec::new();
        for path in rs_files {
            if SELF_TEST_SURFACES
                .iter()
                .any(|name| path.file_name().is_some_and(|f| f == *name))
            {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            let display = path
                .strip_prefix(&src_root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let mut issues = Vec::new();
            scan_markdown(&display, &content, &cli_command, &mut issues);
            for issue in issues {
                // The regression class is folded/deleted *top-level*
                // verbs (gc/monitor/prune/checkout → maintenance …/
                // switch). Subverb/flag drift in source strings is a
                // separate, pre-existing concern and out of scope here.
                //
                // A backslash in the captured invocation means the
                // backtick code-span wrapped across a Rust `\n\` string
                // continuation, so the line-by-line extractor only saw a
                // fragment (e.g. `heddle help\n\`). That's a scan
                // artifact on an otherwise-valid reference, not a stale
                // verb — a real CLI verb never contains a backslash.
                if matches!(issue.kind, IssueKind::UnknownVerb) && !issue.invocation.contains('\\')
                {
                    stale.push(format!("{}:{} — {}", issue.file, issue.line, issue.detail));
                }
            }
        }

        assert!(
            stale.is_empty(),
            "source strings reference verbs that are not current CLI commands \
             (a folded/deleted verb left a stale `heddle <verb>` reference — \
             update it to the canonical spelling):\n{}",
            stale.join("\n"),
        );
    }

    #[test]
    fn detects_invalid_workspace_value() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Try `heddle start probe --workspace ephemeral` to see.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::InvalidFlagValue)
                    && i.invocation.contains("--workspace ephemeral"))
        );
    }

    #[test]
    fn detects_invalid_finite_value_from_catalog_metadata() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Use `heddle context set --path src/lib.rs --kind warning -m note`.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::InvalidFlagValue)
                    && i.detail.contains("--kind warning")),
            "expected catalog-derived invalid --kind value, got: {:?}",
            issues
        );
    }

    #[test]
    fn accepts_global_flags_from_catalog_metadata() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Inspect `heddle status --output json --repo .`.",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty(), "expected no drift, got: {:?}", issues);
    }

    #[test]
    fn accepts_long_aliases_for_catalog_options() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Install `heddle integration install --harness-install-scope user`.",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty(), "expected no drift, got: {:?}", issues);
    }

    #[test]
    fn accepts_catalog_option_aliases() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Install `heddle integration install codex --harness-install-scope repo`.",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty(), "expected no drift, got: {:?}", issues);
    }

    #[test]
    fn does_not_validate_boolean_flags_as_finite_value_options() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Inspect `heddle log --graph main`.",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty(), "expected no drift, got: {:?}", issues);
    }

    #[test]
    fn rejects_retired_commands_even_if_a_stale_parser_variant_survives() {
        for invocation in [
            "`heddle checkpoint -m save`",
            "`heddle actor list`",
            "`heddle session start`",
        ] {
            let mut issues = Vec::new();
            scan_markdown("test.md", invocation, &cli(), &mut issues);
            assert!(
                issues
                    .iter()
                    .any(|issue| matches!(issue.kind, IssueKind::RetiredCommand)),
                "expected retired-command finding for {invocation}: {issues:?}",
            );
        }
    }

    #[test]
    fn rejects_retired_commands_in_plain_prose() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Agents should run heddle checkpoint before review.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|issue| matches!(issue.kind, IssueKind::RetiredCommand)),
            "plain prose must not bypass semantic certification: {issues:?}",
        );
    }

    #[test]
    fn historical_adr_may_name_retired_checkpoint_but_readme_guidance_may_not() {
        let prose = "Migration history: run heddle checkpoint in the old interface.";
        let mut adr_issues = Vec::new();
        scan_markdown("docs/adr/0000-history.md", prose, &cli(), &mut adr_issues);
        assert!(
            adr_issues.is_empty(),
            "historical design records preserve decision context: {adr_issues:?}",
        );

        let mut readme_issues = Vec::new();
        scan_markdown("README.md", prose, &cli(), &mut readme_issues);
        assert!(
            readme_issues
                .iter()
                .any(|issue| matches!(issue.kind, IssueKind::RetiredCommand)),
            "current guidance must reject the retired command: {readme_issues:?}",
        );
    }

    #[test]
    fn rejects_authority_inconsistent_recommendations() {
        for invocation in [
            "`heddle fetch origin`",
            "`heddle thread switch main`",
            "`heddle land feature --push`",
            "`heddle land feature --publish`",
        ] {
            let mut issues = Vec::new();
            scan_markdown("test.md", invocation, &cli(), &mut issues);
            assert!(
                issues
                    .iter()
                    .any(|issue| matches!(issue.kind, IssueKind::AuthorityConflict)),
                "expected authority-conflict finding for {invocation}: {issues:?}",
            );
        }
    }

    #[test]
    fn does_not_reject_non_finite_context_scope_values() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Use `heddle context set --path src/lib.rs --scope symbol:foo --kind rationale -m note`.",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty(), "expected no drift, got: {:?}", issues);
    }

    #[test]
    fn detects_unknown_verb() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Run `heddle frobnicate --foo`.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::UnknownVerb))
        );
    }

    #[test]
    fn planned_marker_skips_only_next_inline_invocation_line() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "<!-- doctor-docs:planned -->\n\
             Planned command: `heddle frobnicate --foo`.\n\
             Real drift: `heddle unsupported --bar`.\n",
            &cli(),
            &mut issues,
        );
        assert_eq!(
            issues.len(),
            1,
            "planned marker should skip exactly one content line; got: {:?}",
            issues
        );
        assert!(
            issues[0].invocation.contains("heddle unsupported"),
            "unmarked drift should remain checked; got: {:?}",
            issues
        );
    }

    #[test]
    fn planned_marker_skips_fenced_invocations() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "```sh doctor-docs:planned\n\
             heddle frobnicate --foo\n\
             ```\n\
             `heddle status --output json`\n",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty(), "expected no drift, got: {:?}", issues);
    }

    #[test]
    fn detects_unknown_flag_on_verb() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            // `--bogus-flag` doesn't exist anywhere; `thread marker delete`
            // only takes a positional name plus a `--prefix`.
            "Use `heddle thread marker delete --bogus-flag failed-` to clean.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::UnknownFlag)),
            "expected at least one UnknownFlag issue, got: {:?}",
            issues
        );
    }

    #[test]
    fn hidden_but_registered_flag_is_not_drift() {
        // heddle#278 r6 (cid 3327633095): `--help-agent` is `hide = true`,
        // so it's dropped from the everyday catalog — but it's still a
        // registered clap arg. Docs that reference `heddle capture
        // --help-agent` (e.g. personas.md) must NOT be flagged as drift.
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Run `heddle capture --help-agent` to reveal the agent flags.",
            &cli(),
            &mut issues,
        );
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::UnknownFlag)),
            "hidden-but-registered `--help-agent` must not be drift, got: {:?}",
            issues
        );
    }

    #[test]
    fn detects_materialized_misnomer_on_start() {
        // The `--materialized` flag was renamed; `heddle start
        // --materialized` should now be flagged.
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "`heddle start <name> --materialized --path <dir>` is the form.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::UnknownFlag)),
            "expected --materialized to be flagged as unknown, got: {:?}",
            issues
        );
    }

    #[test]
    fn accepts_valid_invocations() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "We use `heddle start <name> --path <dir>` here.\n\
             Also `heddle context set --path X --scope file --kind rationale -m \"y\"`.\n\
             And `heddle thread marker delete failed-build` works fine.\n",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty(), "expected no issues, got: {:?}", issues);
    }

    #[test]
    fn ignores_prose_mentions() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "When using heddle context set without proper backticks the checker should ignore.",
            &cli(),
            &mut issues,
        );
        assert!(issues.is_empty());
    }

    #[test]
    fn detects_invalid_kind_value() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Try `heddle context set --path X --scope file --kind reasoning -m foo`.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::InvalidFlagValue))
        );
    }

    #[test]
    fn parses_fenced_code_block() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "```bash\nheddle start probe --workspace ephemeral\n```\n",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::InvalidFlagValue))
        );
    }

    #[test]
    fn rejects_retired_support_verb() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Hosted builds expose `heddle support grant --help` for operators.",
            &cli(),
            &mut issues,
        );
        assert!(
            issues
                .iter()
                .any(|issue| matches!(issue.kind, IssueKind::RetiredCommand)),
            "got: {issues:?}"
        );
    }

    #[test]
    fn skips_placeholder_values() {
        let mut issues = Vec::new();
        scan_markdown(
            "test.md",
            "Run `heddle start <name> --workspace <mode>`.",
            &cli(),
            &mut issues,
        );
        // Should NOT flag `<mode>` as invalid.
        assert!(issues.is_empty(), "got: {:?}", issues);
    }
}
