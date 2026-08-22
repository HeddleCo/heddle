// SPDX-License-Identifier: Apache-2.0
//! Ranked-list help: one quiet screen, topic pages on demand.
//!
//! The Heddle CLI's default `heddle help` renders ONE ranked list: the
//! locked everyday verbs from the 2026-08-19 design lock (heddle#1458)
//! as the head, then every remaining non-hidden root by contract
//! help_rank. There is no second view — depth lives in
//! `heddle <verb> --help` and topic pages (`heddle help <topic>`),
//! which are not verbs. Per-verb help continues to derive from clap
//! doc-comments.
//!
//! # Cultural deliverable
//!
//! The default help is **curated, not auto-generated**. Rank and
//! visibility metadata live in the command contract table so human
//! help and machine command catalog output cannot drift. See
//! `AGENTS.md` "CLI surface curation" for the full doctrine.

/// Verbs that show in `heddle help`, in command-contract order. Blurbs
/// are looked up at print time from the command catalog.
pub fn everyday_verbs() -> Vec<&'static str> {
    crate::cli::commands::root_commands_for_help_visibility("everyday")
}

/// Head-of-list contract: the locked everyday verbs lead `heddle help`,
/// ordered by contract help_rank. Everything else on the screen follows
/// ranked by the same key. Umbrella nouns do not count as one; this is
/// display order, not a fold of the live parser to these 23.
pub const LOCKED_EVERYDAY_VERBS: &[&str] = &[
    "init", "clone", "status", "diff", "capture", "start", "ready", "land", "undo", "pull", "push",
    "resolve", "continue", "log", "show", "query", "review", "discuss", "context", "whoami",
    "daemon", "doctor", "help",
];

/// The ranked first-screen list. Head: the locked everyday verbs in
/// contract order. Tail: every remaining non-hidden root by the same
/// rank key, so nothing is reachable only by reading source.
///
/// Feature-gated verbs with no catalog summary are skipped at print
/// time; they stay in the list here so the ORDER is stable across
/// builds.
fn ranked_help_roots(catalog: &crate::cli::commands::CommandCatalogOutput) -> Vec<&'static str> {
    let rank = |verb: &str| {
        catalog
            .command_by_display(verb)
            .map(|entry| entry.help_rank)
            .unwrap_or(u16::MAX)
    };
    let mut head: Vec<&'static str> = LOCKED_EVERYDAY_VERBS.to_vec();
    head.sort_by_key(|verb| rank(verb));
    for verb in crate::cli::commands::ranked_visible_roots() {
        if !head.contains(&verb) {
            head.push(verb);
        }
    }
    head
}

/// Discover source authority for first-screen help. Fail closed to Native
/// (hide overlay `commit`) when no repository is proven.
fn probed_source_authority() -> repo::RepositorySourceAuthority {
    let Ok(cwd) = std::env::current_dir() else {
        return repo::RepositorySourceAuthority::Native;
    };
    let Some(root) = repo::discover_heddle_root(&cwd) else {
        return repo::RepositorySourceAuthority::Native;
    };
    match repo::RepoConfig::load_for_repository(&root.join(".heddle").join("config.toml")) {
        Ok(config) => config.repository.source_authority,
        Err(_) => repo::RepositorySourceAuthority::Native,
    }
}

fn write_first_screen(out: &mut String, authority: repo::RepositorySourceAuthority) {
    use std::fmt::Write;

    use verbs::source_authority::{SourceAction, SourceAuthorityActions};

    let catalog = crate::cli::commands::build_command_catalog();
    let actions = SourceAuthorityActions::new(authority);
    let capture = actions.display(SourceAction::Capture);

    let _ = writeln!(out, "Heddle — agent-native version control");
    let _ = writeln!(out);
    for name in ranked_help_roots(&catalog) {
        let blurb = catalog_summary(&catalog, name);
        if blurb.is_empty() {
            continue;
        }
        let _ = writeln!(out, "  {:<12}  {}", name, blurb);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Save: heddle init -> {capture}");
    let _ = writeln!(
        out,
        "Isolated work: heddle start <name> --path ../<name> -> {capture} -> heddle ready -> heddle land"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Start here: `heddle init`, `heddle clone`, or `heddle capture`."
    );
    let _ = writeln!(
        out,
        "Tab-complete: `heddle completions bash` (also zsh, fish)."
    );
    let _ = writeln!(
        out,
        "In Git Overlay, Heddle uses its embedded Sley engine to operate directly on the checkout's `.git`."
    );
    let _ = writeln!(
        out,
        "Coming from Git? Run `heddle help git-concepts` for the concept map."
    );
    let _ = writeln!(out);
    // The ONE place the --output machine-contract blurb is stated
    // in full on a help screen; per-command --help carries only the
    // one-line global flag plus the `heddle help output-formats`
    // breadcrumb (heddle#652).
    let _ = writeln!(
        out,
        "Output: text is the default; pass `--output json` for the \
         full machine contract (stable `output_kind`, exit codes, recovery \
         templates), or `--output json-compact` for the decision surface \
         only (fewer tokens, same `output_kind`). No TTY/pipe auto-detection. \
         Details: `heddle help output-formats`."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Run `heddle help model` for the short mental model, \
         or `heddle help <topic>` for a topic page (e.g. `git-concepts`, \
         `git-overlay`, \
         `threads`, `daemon`, `signals`, `git-projection`, `operation-ids`, \
         `remotes`, `output-formats`, `ignore`/`heddleignore`, `git-dependencies`)."
    );
}

/// Look up the catalog summary for a top-level command. Returns an empty
/// string when the verb is feature-gated out of the current build.
fn catalog_summary(catalog: &crate::cli::commands::CommandCatalogOutput, verb: &str) -> String {
    catalog
        .command_by_display(verb)
        .map(|entry| entry.summary.clone())
        .unwrap_or_default()
}

/// Entry point for the `Commands::Help { topics }` dispatch arm
/// AND the bare-help intercept in `main.rs`. Routes between the everyday
/// first screen, topic pages, and clap-derived help for command paths
/// without a dedicated topic.
///
/// All output goes to stdout (this is help, not diagnostic). Returns
/// `Ok(())` even for unknown topics; the printer surfaces the
/// suggestion text rather than erroring.
pub fn print_help(cmd: &clap::Command, topic: &[String]) -> std::io::Result<()> {
    crate::cli::render::write_stdout(&render_help(cmd, topic))
        .map_err(|err| std::io::Error::other(err.to_string()))
}

/// Render the curated help surface to a `String` instead of stdout.
///
/// [`print_help`] is a thin `write_stdout(&render_help(..))` wrapper over
/// this, so the bytes are identical. Extracted so in-process tests can
/// assert on help prose without spawning the binary (HeddleCo/heddle#381).
pub fn render_help(cmd: &clap::Command, topic: &[String]) -> String {
    render_help_for_authority(cmd, topic, probed_source_authority())
}

/// Render curated help for an explicit source authority.
///
/// Topic pages stay authority-independent. The empty-topic first screen
/// hides overlay `commit` unless [`RepositorySourceAuthority::GitOverlay`].
pub fn render_help_for_authority(
    cmd: &clap::Command,
    topic: &[String],
    authority: repo::RepositorySourceAuthority,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    match topic {
        [] => write_first_screen(&mut out, authority),
        [name] if topic_text(name).is_some() => {
            let _ = writeln!(out, "{}", topic_text(name).expect("checked above"));
        }
        path => {
            if let Some(mut subcommand) = help_command_for_path(cmd, path) {
                // `heddle help <command path>` falls through to that
                // command's clap-derived help so the contract on
                // `Commands::Help` holds for nested public paths too.
                let _ = write!(out, "{}", subcommand.render_help());
            } else {
                let name = path.join(" ");
                let _ = writeln!(
                    out,
                    "no topic or command '{name}'. Run `heddle help` for the \
                     everyday surface, or `heddle help model` for the mental model."
                );
            }
        }
    }
    out
}

pub fn print_direct_help_for_raw(
    cmd: &clap::Command,
    raw: &[String],
) -> Option<std::io::Result<()>> {
    let rendered = render_direct_help_for_raw(cmd, raw)?;
    Some(
        crate::cli::render::write_stdout(&rendered)
            .map_err(|err| std::io::Error::other(err.to_string())),
    )
}

/// Render the `heddle <path> --help` pre-parse help to a `String`.
///
/// In-process sibling of [`print_direct_help_for_raw`]; the printer is a
/// `write_stdout` wrapper over this so the bytes match (HeddleCo/heddle#381).
pub fn render_direct_help_for_raw(cmd: &clap::Command, raw: &[String]) -> Option<String> {
    let path = command_path_from_raw_help_request(cmd, raw)?;
    Some(match help_command_for_path(cmd, &path) {
        Some(mut subcommand) => {
            // clap's long renderer unconditionally uses the spaced
            // next-line layout (one blank line per flag), tripling the
            // height of helps whose flags are all one-liners. Render the
            // compact layout whenever the long layout would add no
            // information; commands with real long-form flag docs keep
            // the spaced layout so their exposition stays readable
            // (heddle#652).
            if command_has_long_help_content(&subcommand) {
                subcommand.render_long_help().to_string()
            } else {
                subcommand.render_help().to_string()
            }
        }
        None => render_help(cmd, &path),
    })
}

/// Whether `command`'s help carries long-form content that clap's compact
/// (`-h`-style) renderer would drop: a long about, a long after-help, or
/// a visible argument with a multi-paragraph long help or per-value docs.
/// When this is false the compact and spaced layouts carry identical
/// information, so [`render_direct_help_for_raw`] picks the compact one.
fn command_has_long_help_content(command: &clap::Command) -> bool {
    command.get_long_about().is_some()
        || command.get_after_long_help().is_some()
        || command.get_arguments().any(|arg| {
            !arg.is_hide_set()
                && (arg.get_long_help().is_some()
                    || arg
                        .get_possible_values()
                        .iter()
                        .any(|value| value.get_help().is_some()))
        })
}

/// Clap arg ids of the agent-automation flags on `capture` that are
/// `hide = true` in the everyday surface. Kept in sync with
/// `SnapshotArgs` (see `cli_args/commands_args.rs`); the
/// `capture_agent_flags_hidden_by_default_revealed_on_demand` test fails
/// if an id here no longer maps to a hidden `capture` arg.
const CAPTURE_AGENT_FLAG_IDS: &[&str] = &[
    "agent_provider",
    "agent_model",
    "agent_session",
    "agent_segment",
    "policy",
    "no_policy",
    "no_agent",
    "split",
    "into",
    "paths",
];

/// Return a copy of `command` with the agent-automation flags un-hidden,
/// so `print_long_help` renders them. Used by `capture --help-agent`.
fn reveal_capture_agent_flags(command: clap::Command) -> clap::Command {
    command.mut_args(|arg| {
        if CAPTURE_AGENT_FLAG_IDS.contains(&arg.get_id().as_str()) {
            arg.hide(false)
        } else {
            arg
        }
    })
}

/// Render `capture`'s clap-derived help with the hidden agent-automation
/// flags revealed. Called from the `Commands::Capture` dispatch arm once
/// clap has parsed `--help-agent` (a first-class flag on `capture`).
///
/// Because clap owns the parsing, every global spelling it accepts —
/// `-C <path>`, `--output <fmt>`, clustered `-vC <path>`, attached forms, in
/// any legal position — is handled natively; there is no hand-rolled
/// pre-parse token scan to keep in sync with clap's grammar.
pub fn print_capture_agent_help(cmd: &clap::Command) -> std::io::Result<()> {
    crate::cli::render::write_stdout(&render_capture_agent_help(cmd))
        .map_err(|err| std::io::Error::other(err.to_string()))
}

/// Render `capture --help-agent` to a `String` (the reveal-help variant).
///
/// In-process sibling of [`print_capture_agent_help`]; the printer is a
/// `write_stdout` wrapper over this so the bytes match (HeddleCo/heddle#381).
pub fn render_capture_agent_help(cmd: &clap::Command) -> String {
    let capture = find_subcommand_or_alias(cmd, "capture")
        .expect("capture subcommand exists in the clap command tree");
    let bin_name = format!("{} {}", cmd.get_name(), capture.get_name());
    let mut help = reveal_capture_agent_flags(capture.clone()).bin_name(bin_name);
    help.render_long_help().to_string()
}

fn help_command_for_path(cmd: &clap::Command, path: &[String]) -> Option<clap::Command> {
    if path.is_empty() {
        return None;
    }

    let mut current = cmd;
    let mut bin_name = cmd.get_name().to_string();
    let mut canonical_path = Vec::new();
    for part in path {
        let subcommand = find_subcommand_or_alias(current, part)?;
        bin_name.push(' ');
        bin_name.push_str(part);
        canonical_path.push(subcommand.get_name().to_string());
        current = subcommand;
    }

    let mut help = current.clone().bin_name(bin_name);
    for arg in cmd
        .get_arguments()
        .filter(|arg| arg.is_global_set() && !arg.is_hide_set())
    {
        help = help.arg(arg.clone());
    }
    if crate::cli::commands::command_runtime_contract(&canonical_path.join(" "))
        .is_some_and(|contract| contract.supports_op_id)
        && let Some(arg) = cmd
            .get_arguments()
            .find(|arg| arg.get_long() == Some("op-id"))
    {
        help = help.arg(arg.clone().hide(false).value_name("UUID"));
    }
    Some(help)
}

/// Whether `token` names an option on `command` — in either its `--long`
/// or `-x` separated spelling — and, if so, whether the following token is
/// that option's value. Used by the `heddle <path> --help` pre-parse scan
/// (`command_path_from_raw_help_request`) so a valued global before the verb
/// (`-C <path>`, `--output <fmt>`) isn't mistaken for the command path.
/// Derived from clap's own arg definitions, so short and long forms stay
/// covered as globals are added or renamed.
///
/// Only the *separated* spellings (`-C path`, `--output fmt`) need a
/// following-value skip. Attached spellings (`--output=fmt`, `-Cpath`,
/// `-C=path`) carry the value in the same token, so they never set
/// `skip_next`; the leading-dash fall-through in the scan loop already
/// drops them without consuming the next token.
fn global_option_takes_value(command: &clap::Command, token: &str) -> Option<bool> {
    command
        .get_arguments()
        .find(|arg| {
            arg.get_long()
                .is_some_and(|long| token == format!("--{long}"))
                || arg
                    .get_short()
                    .is_some_and(|short| token == format!("-{short}"))
        })
        .map(|arg| arg.get_action().takes_values())
}

fn find_subcommand_or_alias<'a>(
    command: &'a clap::Command,
    name: &str,
) -> Option<&'a clap::Command> {
    command.find_subcommand(name).or_else(|| {
        command
            .get_subcommands()
            .find(|subcommand| subcommand.get_all_aliases().any(|alias| alias == name))
    })
}

fn command_path_from_raw_help_request(cmd: &clap::Command, raw: &[String]) -> Option<Vec<String>> {
    if !raw.iter().any(|arg| arg == "--help" || arg == "-h") {
        return None;
    }
    if raw
        .iter()
        .all(|arg| arg == "--help" || arg == "-h" || arg.starts_with('-'))
    {
        return None;
    }

    let mut current = cmd;
    let mut path = Vec::new();
    let mut skip_next = false;
    for token in raw {
        if skip_next {
            skip_next = false;
            continue;
        }
        if token == "--help" || token == "-h" {
            continue;
        }
        if let Some(takes_value) = global_option_takes_value(current, token) {
            skip_next = takes_value;
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        if let Some(subcommand) = find_subcommand_or_alias(current, token) {
            path.push(subcommand.get_name().to_string());
            current = subcommand;
        }
    }

    (!path.is_empty()).then_some(path)
}

/// Render the help that `heddle <args>` would print, **in-process**, for the
/// help-shaped argv forms — without spawning the binary.
///
/// `args` is the argv *after* the program name (e.g. `["clone", "--help"]`,
/// `["help", "threads"]`, `["capture", "--help-agent"]`). Returns `Some(text)`
/// with the exact bytes the binary writes to stdout for that request, or
/// `None` when the argv is not a pure help request this renderer serves (the
/// caller should fall back to spawning the binary).
///
/// This mirrors `main.rs`'s help-dispatch routing precisely: the bare-help
/// intercept, the `heddle <path> --help` pre-parse, the `capture --help-agent`
/// reveal, and the `heddle help <topics>` arm. Because the underlying printers
/// are now `write_stdout(&render_*(..))` wrappers, the in-process text is
/// byte-identical to the spawned binary's stdout — only the execution
/// mechanism differs (HeddleCo/heddle#381). Topic and per-verb help is
/// repo-, cwd-, and env-independent. The empty-topic first screen probes
/// source authority and fails closed to Native when no repository is proven.
pub fn render_for_args(args: &[&str]) -> Option<String> {
    use clap::{CommandFactory, Parser};

    use crate::cli::cli_args::{Cli, Commands};

    let command = Cli::command();
    let raw: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();

    // Bare-help shapes: `heddle`, `heddle help`. (`--help`/`-h` alone are
    // clap-driven help that exits during parse; serve them too since they
    // render the curated everyday surface.)
    if raw.is_empty() || raw == ["--help"] || raw == ["-h"] || raw == ["help"] {
        return Some(render_help(&command, &[]));
    }

    // `heddle <path> --help` pre-parse direct help (e.g. `clone --help`,
    // `bridge git import --help`).
    if let Some(rendered) = render_direct_help_for_raw(&command, &raw) {
        return Some(rendered);
    }

    // `capture --help-agent` reveal: clap owns the parse, so every global
    // spelling it accepts is handled natively.
    if let Ok(cli) = Cli::try_parse_from(std::iter::once("heddle".to_string()).chain(raw.clone()))
        && let Commands::Help { topics } = &cli.command
    {
        // `heddle help <topics>` — curated topic / command-path help.
        return Some(render_help(&command, topics));
    }
    if let Ok(cli) = Cli::try_parse_from(std::iter::once("heddle".to_string()).chain(raw.clone()))
        && let Commands::Capture(args) = &cli.command
        && args.help_agent
    {
        return Some(render_capture_agent_help(&command));
    }

    None
}

/// Static per-topic help. Topics are addressed via `heddle help <topic>`.
pub fn topic_text(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "agent-flags" => AGENT_FLAGS_TOPIC,
        "config" | "environment" => CONFIG_TOPIC,
        "agent" | "daemon" => DAEMON_TOPIC,
        "output-formats" | "output-format" | "output" => OUTPUT_FORMATS_TOPIC,
        "clone" => CLONE_TOPIC,
        "git-overlay" => GIT_OVERLAY_TOPIC,
        "git-concepts" | "git-concept-map" | "git-veteran" => GIT_CONCEPTS_TOPIC,
        "model" | "mental-model" | "concepts" => MODEL_TOPIC,
        "threads" => THREADS_TOPIC,
        "operation-ids" | "idempotency" => OPERATION_IDS_TOPIC,
        "remotes" => REMOTES_TOPIC,
        "git-dependencies" | "git-deps" | "git-dependency" => GIT_DEPENDENCIES_TOPIC,
        "heddleignore" | "ignore" => HEDDLEIGNORE_TOPIC,
        "review" => REVIEW_TOPIC,
        "discuss" | "discussions" => DISCUSS_TOPIC,
        "git-projection" | "git-projections" | "footer" | "notes" => GIT_PROJECTION_TOPIC,
        "signals" | "risk-signals" => SIGNALS_TOPIC,
        _ => return None,
    })
}

const HEDDLEIGNORE_TOPIC: &str = include_str!("../../../../docs/heddleignore.md");

/// Configuration environment + principal resolution. This reference
/// material used to live on the retired `heddle help advanced` page;
/// `heddle help config` keeps it reachable from the ranked screen.
const CONFIG_TOPIC: &str = r#"Configuration environment and principal resolution.

Configuration environment:
  HEDDLE_HOME    Relocates global Heddle state (credentials, device identity,
                 session state, and the default user config).
  HEDDLE_CONFIG  Overrides the user config file. Takes precedence over
                 HEDDLE_HOME; otherwise HEDDLE_HOME uses <path>/config.toml.

Principal resolution (highest first): HEDDLE_PRINCIPAL_NAME and
HEDDLE_PRINCIPAL_EMAIL, repository config, Git config, user config, then
Unknown <unknown@example.com>. Init, status, and capture use this same order.
"#;

// The single full statement of the --output machine contract (plus the
// one-paragraph version on the top-level `heddle help`). The global
// `--output` flag's own help is one line pointing here, so the contract
// is not restated on every command's --help (heddle#652).
const OUTPUT_FORMATS_TOPIC: &str = r#"Output formats — `--output text | json | json-compact`.

`text` is the default, always. There is no TTY/pipe auto-detection — the
default never switches under you, so scripts and humans see the same thing
until a flag says otherwise.

`--output json` emits the full machine contract: a stable `output_kind`
discriminator, exit codes, and recovery templates. The per-verb schema
catalog: `heddle help --output json`.

`--output json-compact` emits only the decision-surface fields —
`output_kind`, `status`/`coordination_status`, `blockers`, `next_action`,
`changed_paths`, `conflicts` — fewer tokens, same `output_kind`, so callers
can still dispatch on it. Commands advertise `supports_json_compact` in the
command catalog.

Related: `heddle help operation-ids` for idempotent retries, `heddle help
agent-flags` for capture attribution overrides.
"#;

// `heddle clone --help` keeps the signature + flags + a one-screen
// summary; this topic carries the full default-thread fallback chain and
// --depth exposition that used to bloat the after-help (heddle#652).
const CLONE_TOPIC: &str = r#"Cloning — Git Overlay and native Heddle repositories.

    heddle clone <remote> <dir> [--thread <name>] [--depth <n>]

Run `heddle clone --help` for the flag list.

# Repository authority

- A Git source is streamed by Sley directly into the destination `.git`, then
  initialized as Git Overlay. No Git executable or `.heddle/git` mirror is used.
- A native source is cloned into Heddle-owned storage.
- Native clones follow the remote default thread; pass `--thread <name>`
  to override.
- Clone never prompts.

# Shallow clones (--depth)

--depth 0 (the default) clones full history. --depth N fetches only the
tip plus N generations of ancestry (--depth 1: the tip plus its immediate parents),
so `heddle log` stops at the depth boundary; history older than that is
not present locally — re-clone at a greater --depth (or --depth 0) to
obtain it.

Depth controls native Heddle history extent only — how many states the clone fetches —
and says nothing about object contents. Whether a state's blobs are
present locally or fetched lazily is a separate concern that `--depth`
never governs. Git Overlay clones ingest full history and reject partial-history
options. Advanced/planned flags `--lazy` and `--filter blob:none`
skip blob content and hydrate it on demand for hosted/network Heddle
remotes; local clone paths reject them today.

See `heddle help threads` for the thread model and `heddle help remotes`
for remote management.
"#;

const AGENT_FLAGS_TOPIC: &str = r#"Agent automation flags for `heddle capture`.

These flags are hidden from the everyday `heddle capture --help` so it stays
terse for human use. They let an automated caller override agent attribution
and split captures across threads. Run `heddle capture --help-agent` to see
them inline in capture's own help.

Attribution overrides:

  --agent-provider <NAME>   Override HEDDLE_AGENT_PROVIDER.
  --agent-model <NAME>      Override HEDDLE_AGENT_MODEL.
  --agent-session <ID>      Set the session id for this capture.
  --agent-segment <ID>      Set the session segment for this capture.
  --policy <ID>             Override HEDDLE_AGENT_POLICY.
  --no-policy               Omit policy attribution.
  --no-agent                Omit agent attribution.

Path splitting (no env equivalent):

  --split                   Split selected paths into another thread instead of
                            capturing the whole worktree.
  --into <THREAD>           Target thread when using --split.
  --path <PATH>             Repository-relative path prefix to include with
                            --split (repeatable).

Attribution precedence (highest first): explicit flag, active thread actor,
supported agent env var, harness probe, active session, user config, repo config. See
`crates/cli/src/cli/commands/snapshot.rs` for the full cascade.
"#;

const DAEMON_TOPIC: &str = "The mount daemon owns virtualized workspace sessions.\n\
\n\
`heddle daemon`        — FUSE mount-daemon control plane. Owns FUSE sessions for\n\
                         `--workspace virtualized --daemon` threads. Linux only.\n\
                         Subcommands: serve | status | stop.\n";

const MODEL_TOPIC: &str = r#"Heddle mental model — the everyday loop in one screen.

Heddle is built around saved states and isolated threads. Git compatibility is
an output and interop layer, not the thing you have to think about first.

Core nouns:

- State: a captured tree with a stable change id, attribution, intent, and
  provenance. States are what `log`, `show`, `diff`, `undo`, and `query` can
  reason about.
- Thread: a named line of work with its own checkout and captured history.
  Use it for risky edits, agent work, or parallel experiments without
  serializing everything through one checkout.
- Capture: the save. Provenance, undo, and review start here.
- Annotation: durable context that lives with the code (`heddle context`).
- Discussion: scoped chats that open, get turns, and resolve (`heddle discuss`).

Everyday verbs:

    heddle init / clone
    heddle status
    heddle diff
    heddle capture -m "..."
    heddle start <name> --path ../<name>
    heddle ready
    heddle land
    heddle undo
    heddle pull / push
    heddle resolve / continue
    heddle log / show / query
    heddle review / discuss / context
    heddle whoami
    heddle daemon
    heddle doctor
    heddle help

Save is `capture`. The first-screen loop does not run `capture` then `commit`.

Existing Git checkout:

    heddle status
    heddle init                  # initialize Heddle metadata; Git commits stay in .git
    heddle capture -m "..."
    heddle doctor

If a command refuses, read the first `Next:` line. Heddle fails closed when it
cannot prove the move is safe.

Restore:

Heddle does not restore one file from a saved state (no restore/checkout/reset).
Materialize one state in a new checkout with `heddle start <name> --from <state>
--path <dir>`. Invert one state onto this worktree with `heddle revert <state>`.
Restore the tree preserved by the last undo with `heddle undo --recover`.
"#;

const GIT_CONCEPTS_TOPIC: &str = r#"Git and Heddle own different layers.

In a Git Overlay repository, the checkout's real `.git` owns commits, refs,
packs, the index, and worktree state. Heddle's thin Git surface — `clone`,
`commit`, `pull`, `push`, and `remote` — uses the embedded Sley engine directly
against that store. Heddle owns coordination and durable metadata in `.heddle`:
captures, provenance, threads, readiness, review, and safe landing. Normal
overlay operation neither creates nor uses `.heddle/git`. Legacy explicit Git
Projection maintenance can still use an existing Bridge Mirror while its
retirement is completed.

Use `heddle init` to add that sidecar to an existing Git checkout. Use
`heddle adopt` when you want one atomic transition that imports source history,
makes Heddle the repository authority, and enables the full native feature set.

Common mappings:

| Intent | Git Overlay | Native Heddle |
|--------|-------------|---------------|
| Save source history | `heddle capture`, then `heddle commit` | `heddle capture` |
| Isolate coordinated work | `heddle start` | `heddle start` |
| Record a granular Heddle savepoint | `heddle capture` | `heddle capture` |
| Check integration readiness | `heddle ready` | `heddle ready` |
| Integrate a managed thread | `heddle land` | `heddle land` |
| Synchronize source | `heddle pull` / `heddle push` | `heddle pull` / `heddle push` |
| Configure remotes | `heddle remote` | `heddle remote` |
| Inspect source history | another Git-compatible client | `heddle log` |

Heddle intentionally does not reproduce the full Git command surface. An
optional Git-compatible client can perform unsupported Git operations against
the same `.git`; it is not a Heddle dependency. Explicit `bridge git import`,
`bridge git export`, and `sync git` translate data between authorities. After `heddle adopt`,
the retained `.git` is an explicit Git Projection adapter; it no longer selects
repository source authority.
"#;

const THREADS_TOPIC: &str = "Threads — Heddle's unit of in-progress work.\n\
\n\
A thread is a named line of work with its own checkout, its own captured\n\
history, and a target it eventually merges into. It is not a Git branch. In\n\
Git Overlay, the branch remains part of Git source storage; the Heddle thread\n\
owns coordination and captured history. You start isolated work with\n\
`heddle start <name> --path <dir>`, switch\n\
between threads with `heddle thread switch <name>`, and integrate with\n\
`heddle land` (or check readiness without merging via `heddle ready`).\n\
\n\
# Threads vs. git branches\n\
\n\
- A thread carries an isolated checkout (its own directory), captured\n\
  state history, agent/task metadata, a freshness verdict against its\n\
  target, and a workflow state (Ready/Blocked/Merged/...). A git branch\n\
  is just a ref.\n\
- Multiple threads coexist on disk simultaneously. Each thread's working\n\
  tree is its own.\n\
- `heddle capture` records Heddle metadata; `heddle commit` asks Sley to\n\
  write source history directly to `.git` in Git Overlay.\n\
\n\
# Workspace modes (`--workspace`)\n\
\n\
The `--workspace` flag on `heddle start` selects how the thread's\n\
checkout is realized on disk. These are storage strategies, not\n\
workflow states:\n\
\n\
- `materialized` — clonefile/reflink the captured tree into the thread's\n\
  directory (APFS / btrfs / XFS-with-reflinks / bcachefs / ReFS). Real\n\
  `read(2)`-able bytes; ~zero disk cost until the agent diverges blocks.\n\
  Day-one default on reflink-capable hosts.\n\
- `virtualized` — project the captured tree through a content-addressed\n\
  FUSE/FSKit/ProjFS mount. Nothing on disk until the kernel asks.\n\
  Requires the `mount` feature.\n\
- `solid` — full file copies, no shared extents. Strong isolation;\n\
  the right choice on ext4/NTFS hosts that have neither reflinks nor a\n\
  usable mount API.\n\
- `auto` (default) — pick `materialized` when reflinks are available,\n\
  `virtualized` when a mount is available, otherwise `solid`.\n\
\n\
A `solid` thread and a `materialized` thread are interchangeable from\n\
the workflow's point of view — `capture`, `ready`, and `land` behave\n\
identically. The mode only controls bytes-on-disk semantics.\n\
\n\
# Isolated checkout path\n\
\n\
- Use `heddle start <name> --path <dir>` when you want an isolated\n\
  checkout. It creates the thread ref and materializes the checkout in\n\
  one step. `--path` is required when workspace is omitted or `auto`; without it\n\
  start refuses instead of hiding a checkout under `.heddle/threads/<name>/`.\n\
- To stay on this checkout, use `heddle thread create <name>` then\n\
  `heddle thread switch <name>`.\n\
- Advanced split form: `heddle thread create <name>` creates only the\n\
  ref, and `heddle thread promote <name> --path <dir>` materializes it\n\
  later. Use this only when you intentionally need to create the ref\n\
  now and materialize the checkout later.\n\
- `--workspace` on `heddle start` selects byte storage for that checkout;\n\
  it is not a separate workflow path.\n\
\n\
# Sync: stale\n\
\n\
A thread is `current` when its base is the tip of its target, and\n\
`stale` once the target has advanced past it. `heddle status` and\n\
`heddle thread show` print this as `Sync: stale`.\n\
\n\
Resolution paths:\n\
\n\
- `heddle sync` — refresh the current thread onto its target when\n\
  the replay is clean. The fast path for a stale thread with no\n\
  conflicts.\n\
- If `sync` reports conflicts or other blockers, use\n\
  `heddle resolve` or `heddle continue` to handle the conflicts.\n\
- `heddle land` will refresh-then-merge for you when the replay is\n\
  clean; it fails closed when manual resolution is required.\n\
\n\
# Changing active work\n\
\n\
- `heddle thread switch <name>` changes which *thread* is active.\n\
  Each thread has its own checkout; switching may auto-capture\n\
  outstanding work on the thread you're leaving. Pair with the shell\n\
  hook (`heddle shell init`) to auto-cd into the target thread's\n\
  directory.\n\
- In Git Overlay, branch switching remains outside Heddle's deliberately\n\
  narrow Git surface. An optional Git-compatible client can update `.git`\n\
  without changing Heddle's active thread or coordination metadata.\n\
\n\
# Capture and Git commits\n\
\n\
- `heddle capture` records a recoverable Heddle step on the current\n\
  thread — for undo, provenance, and review. Captures are\n\
  fine-grained and accumulate freely as work progresses.\n\
- In Git Overlay, run `heddle commit` when captured source history is ready.\n\
- Agents and tools can take many small captures without producing noisy Git history.\n\
\n\
See also: `heddle help config` for environment overrides,\n\
`heddle thread --help` for the thread subcommand list.\n";

const OPERATION_IDS_TOPIC: &str = "Idempotency — machine retries for supported mutating commands.\n\
\n\
Commands that advertise `supports_op_id: true` in `heddle help --output json`\n\
accept `--op-id <UUID>` or `HEDDLE_OPERATION_ID`. Replaying the same id\n\
with the same body returns the recorded outcome; with a different body it\n\
returns a typed conflict.\n\
\n\
`op_id_behavior: explicit_replay` means the caller must provide the id.\n\
`op_id_behavior: generated_resume` is reserved for commands that also\n\
advertise `persists_op_id: true` and can save a generated id across an\n\
interrupted retry loop. Commands with `op_id_behavior: none` reject --op-id.\n\
\n\
The dedup store is file-backed locally (`.heddle/state/operation_dedup.bin`,\n\
rmp-serde, 7-day default retention) and Postgres-backed in hosted deployments.\n\
\n\
Without an id, dedup is bypassed and the call executes normally. For the\n\
authoritative per-command contract, use `heddle help --output json`.\n";

const REMOTES_TOPIC: &str = r#"Remotes — dispatched by repository source authority.

Common loop:

    heddle remote add origin <url-or-path>
    heddle remote set-default origin
    heddle pull
    heddle push
    heddle verify

Remote values may be Git URLs, hosted endpoints, or local paths. `push` and
`pull` use the default remote unless a positional remote is supplied. In Git
Overlay, Sley reads and edits the repository's Git configuration and streams
objects directly between the remote and `.git`. In Native Heddle, the same
verbs use Heddle transport and storage. The Git executable is not involved.

When a remote action is unsafe, Heddle reports the blocker and one primary next
command for the active source authority. `heddle verify` reports repository
verification state before and after remote operations.
"#;

const GIT_DEPENDENCIES_TOPIC: &str = r#"Git executable dependencies — Heddle does not have one.

Heddle never requires the `git` executable. Sley is its embedded Git engine. In
Git Overlay, `heddle clone`, `commit`, `pull`, `push`, and `remote`
operate on the checkout's real `.git` directly; `.heddle` contains Heddle
metadata, not a second Git object store.

If Heddle detects an externally-started Git sequencer operation, it leaves Git
metadata, refs, index, and worktree files unchanged and reports a Heddle
preservation command. Finish or abort that operation with the optional
Git-compatible client that started it, then run `heddle verify`.

Operations outside Heddle's narrow Git surface can be performed by another
Git-compatible client, but that client is optional and is never invoked by
Heddle. Unsupported capabilities fail closed.

Run `heddle help --output json` to inspect the public command surface, and
`heddle doctor` / `heddle maintenance fsck --full` when a repository reports integrity
or Git Projection state problems.
"#;

const REVIEW_TOPIC: &str = "Review surface — `heddle review show | sign | next | health`.\n\
\n\
`show <state>`    — render the review payload (summary, agent narrative,\n\
                    in-budget signals, anchored discussions).\n\
                    `--all-signals` also surfaces hidden ones.\n\
`sign <state>`    — submit a `read | agent_preview | agent_co_review`\n\
                    signature. `--symbols file:symbol` scopes to\n\
                    specific symbols; default is the whole change.\n\
`next`            — show the next locally discoverable review item, or explain\n\
                    why none is available.\n\
`health [--window N]`\n\
                  — per-module signal fire-rate over the last N states.\n\
\n\
Tick budget: at most 3 signals per state by default. Priority:\n\
invariant_adjacency > self_flagged_uncertainty > pattern_deviation >\n\
novelty > test_reachability.\n";

const DISCUSS_TOPIC: &str = "`heddle discuss open | append | resolve | reopen | list | show`\n\
\n\
Scope: native Heddle only. Discussions are stored in `.heddle` and travel over\n\
`heddle push` / `heddle pull` to a Heddle remote. They are deliberately not\n\
projected into Git — not into `refs/notes/*`, not into a tracked file — so\n\
`git push` and `git clone` do not carry them. In Git Overlay mode discussions\n\
still work; they are local to that working copy, and `heddle discuss open`\n\
says so once. A clone with no `.heddle` reports that no store is present\n\
rather than reporting zero discussions.\n\
\n\
Discussions are stable records in the repository collaboration log. Turns,\n\
resolutions, and reopenings append immutable operations; concurrent turns\n\
converge without rewriting source history. Symbol anchors record the state,\n\
file, and symbol where the discussion began:\n\
\n\
- `resolve <id> --mode by-edit`          with `--state` (defaults to HEAD).\n\
  Records that a subsequent edit addressed the discussion.\n\
- `resolve <id> --mode dismiss`          requires non-empty `--reason`.\n\
- `reopen <id> --reason <text>`          compensates a prior resolution.\n\
\n\
Visibility: `--visibility public|internal|team:NAME|restricted:LABEL|private:LABEL`.\n\
Empty visibility uses the configured discussion visibility policy.\n";

const GIT_OVERLAY_TOPIC: &str = r#"Git Overlay workflow

Use this when `.git` should remain the source authority while Heddle adds
captures, isolated threads, merge previews, undo, provenance, and machine-safe
JSON in `.heddle`. Sley is the embedded Git engine; no Git executable is
required. Normal Git Overlay operation neither creates nor reads `.heddle/git`.

What `.heddle` adds here is local to this working copy. Context annotations
(`heddle context`) and discussions (`heddle discuss`) are a native Heddle
feature: they move over `heddle push` / `heddle pull` to a Heddle remote, and
are not projected into Git. Collaborators who `git clone` this repository get
the source history and no Heddle store — `heddle context list` there reports
that no store is present, not that there are no annotations.

Start in an existing Git checkout:

    heddle status
    heddle init                               # add Heddle metadata
    heddle verify

Save and synchronize ordinary work:

    heddle diff
    heddle capture -m "..."                   # save Heddle metadata and provenance
    heddle commit                             # Sley writes source history to .git
    heddle pull
    heddle push

Isolate risky work:

    heddle start <name> --path ../<name>
    cd ../<name>
    heddle capture -m "..."
    heddle ready
    cd -
    heddle land --thread <name>
    heddle push

Recover or prove state:

    heddle undo
    heddle verify

State-specific recovery:

    Worktree has unsaved edits: heddle capture -m "...", then heddle commit
    Move atomically to the full Native Heddle feature set: heddle adopt --ref <branch>
"#;

const GIT_PROJECTION_TOPIC: &str = r#"Git Projection — translate between Native Heddle and Git.

Git Projection is the explicit interoperability adapter for Native Heddle
repositories. It is not Git Overlay: when `.git` remains authoritative, Sley
operates on that repository directly and normal operation never reads or creates
`.heddle/git`. Heddle does not require the Git executable in either mode.

Move an existing Git repository to Native Heddle atomically:

    heddle adopt --ref <branch>

Translate explicitly without changing source authority:

    heddle bridge git import --path <git-repository> --ref <branch>
    heddle bridge git export --destination <bare-git-repository>
    heddle sync git --path <git-repository>

`bridge git import` and `sync git` accept a local path or Git URL. Omit `--ref` to
import all local branches and tags. `bridge git export` writes a bare Git repository.
Legacy migration and repair may still read an existing Bridge Mirror at
`.heddle/git` while ADR 0042's retirement work remains incomplete.

Export metadata for Git readers:

Every exported commit carries a footer at the tail of the commit message:

    Heddle-State: <state_id>
    Heddle-URL: <hosted_url>/state/<state_id>     (omitted if no hosted URL)
    Heddle-Annotations-Omitted: <count>

This is the durable record — every reader on every host sees it regardless
of remote configuration.

Per-scope annotation drop counts and signal counts ride on the opt-in
Git note at `refs/notes/heddle`. Heddle reads and writes that ref natively;
people who still inspect the repository through another Git client can opt
that client into showing notes, but Heddle itself does not require a Git
executable on the system.
"#;

const SIGNALS_TOPIC: &str = "Risk signals — five modules behind a pure trait.\n\
\n\
- `invariant_adjacency`        — fires when a changed symbol carries an\n\
                                  Invariant or `enforces`-tagged annotation.\n\
- `self_flagged_uncertainty`   — passthrough of agent-emitted self-flags\n\
                                  from the captured state's intent.\n\
- `pattern_deviation`          — fires when a symbol's body diverges\n\
                                  from siblings or the prior version\n\
                                  (tree-sitter token similarity).\n\
- `novelty`                    — fires when a function shape is unique\n\
                                  in the repo corpus.\n\
- `test_reachability`          — fires when no test statically reaches\n\
                                  the changed symbol via tree-sitter\n\
                                  call-graph traversal. The reason text\n\
                                  is honest: this is *not* runtime\n\
                                  coverage.\n\
\n\
Configure under `[review.signals]` in `.heddle/config.toml`. Each module\n\
ships fires-correctly + stays-quiet tests; defaults are conservative\n\
so a fresh repo isn't noisy.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everyday_verbs_carry_the_everyday_catalog_label() {
        let catalog = crate::cli::commands::build_command_catalog();
        for verb in everyday_verbs() {
            assert_eq!(
                crate::cli::commands::command_help_visibility(verb),
                "everyday",
                "{verb}"
            );
            assert!(catalog.command_by_display(verb).is_some(), "{verb}");
        }
    }

    #[test]
    fn topic_text_returns_none_for_unknown() {
        assert!(topic_text("definitely-not-a-topic").is_none());
    }

    #[test]
    fn topic_text_returns_some_for_advertised_topics() {
        for topic in [
            "agent-flags",
            "config",
            "git-concepts",
            "git-concept-map",
            "git-veteran",
            "git-overlay",
            "agent",
            "daemon",
            "threads",
            "model",
            "mental-model",
            "concepts",
            "operation-ids",
            "idempotency",
            "remotes",
            "git-dependencies",
            "heddleignore",
            "ignore",
            "review",
            "discuss",
            "discussions",
            "git-projection",
            "git-projections",
            "footer",
            "notes",
            "signals",
            "risk-signals",
            "output-formats",
            "output-format",
            "output",
            "clone",
        ] {
            assert!(topic_text(topic).is_some(), "{topic}");
        }
    }

    /// Regression: heddle#150. Commands referenced in inline tips and
    /// error messages must remain discoverable on the ranked screen.
    #[test]
    fn ranked_screen_lists_tip_referenced_commands() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let help = render_help_for_authority(&cmd, &[], repo::RepositorySourceAuthority::Native);
        for verb in ["abort", "shell"] {
            assert!(
                help.lines()
                    .any(|line| line.trim_start().starts_with(&format!("{verb}  "))),
                "`{verb}` is referenced in user-facing tips but is not \
                 advertised by `heddle help`"
            );
        }
    }

    /// heddle#1435. Default first screen follows Native source authority:
    /// capture is the save. The ranked tail may list `commit`, but the
    /// save loop never routes through it.
    #[test]
    fn first_screen_teaches_capture_as_the_save() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let help = render_help_for_authority(&cmd, &[], repo::RepositorySourceAuthority::Native);
        assert!(
            help.contains("heddle capture -m"),
            "default first screen still teaches capture: {help}"
        );
        assert!(
            help.contains("Save: heddle init -> heddle capture -m \"...\""),
            "native first screen follows the unborn save path: {help}"
        );
        assert!(
            !help.contains("-> heddle commit ->"),
            "first screen must not teach overlay commit in the save loop: {help}"
        );
    }

    /// heddle#1439. First-screen help names `heddle completions` so tab
    /// scripts are findable without digging through topics.
    #[test]
    fn first_screen_mentions_completions() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let help = render_help_for_authority(&cmd, &[], repo::RepositorySourceAuthority::Native);
        assert!(
            help.contains("heddle completions"),
            "first screen must name the public completions verb: {help}"
        );
        assert!(
            help.contains("heddle completions bash"),
            "first screen should show a concrete install invocation: {help}"
        );
    }

    #[test]
    fn first_screen_does_not_teach_capture_then_commit_on_overlay() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let help =
            render_help_for_authority(&cmd, &[], repo::RepositorySourceAuthority::GitOverlay);
        assert!(
            help.contains("Save: heddle init -> heddle capture -m \"...\""),
            "overlay first screen still teaches capture as the save: {help}"
        );
        assert!(
            !help.contains("write the Git commit") && !help.contains("side effect"),
            "overlay first screen must not claim ready/push write the Git commit: {help}"
        );
        assert!(
            !help.contains("-> heddle commit"),
            "overlay first screen must not teach capture then commit: {help}"
        );
    }

    /// heddle#1458. First screen is the locked everyday surface: the ~23
    /// verbs, no `help advanced`, no capture-then-commit, no default
    /// `land --thread`, no leftover nearby `verify`.
    #[test]
    fn first_screen_is_the_locked_everyday_surface() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let catalog = crate::cli::commands::build_command_catalog();
        for authority in [
            repo::RepositorySourceAuthority::Native,
            repo::RepositorySourceAuthority::GitOverlay,
        ] {
            let help = render_help_for_authority(&cmd, &[], authority);
            assert!(
                !help.contains("heddle help advanced"),
                "first screen must not point at `help advanced`: {help}"
            );
            assert!(
                !help.contains("land --thread"),
                "first screen must not teach default `land --thread`: {help}"
            );
            assert!(
                !help.contains("-> heddle commit"),
                "first screen must not teach capture then commit: {help}"
            );
            assert!(
                !help.contains("Nearby:") && !help.contains("`heddle verify`"),
                "first screen must not park verify as a nearby leftover: {help}"
            );
            for verb in LOCKED_EVERYDAY_VERBS {
                let Some(entry) = catalog.command_by_display(verb) else {
                    continue;
                };
                if entry.summary.is_empty() {
                    continue;
                }
                assert!(
                    help.lines()
                        .any(|line| line.trim_start().starts_with(&format!("{verb}  "))
                            || line.trim_start().starts_with(&format!("{verb}\t"))),
                    "first screen must list everyday verb `{verb}`: {help}"
                );
            }
        }
    }

    /// heddle#1458. `help model` matches the locked everyday surface.
    #[test]
    fn help_model_is_the_locked_everyday_surface() {
        let model = topic_text("model").expect("model topic exists");
        assert!(
            !model.contains("heddle help advanced"),
            "help model must not point at `help advanced`: {model}"
        );
        assert!(
            !model.contains("land --thread"),
            "help model must not teach default `land --thread`: {model}"
        );
        assert!(
            !model.contains("write the Git commit") && !model.contains("heddle commit"),
            "help model must not claim ready/push write-through or teach heddle commit: {model}"
        );
        for verb in ["query", "review", "discuss", "context", "daemon", "whoami"] {
            assert!(
                model.contains(verb),
                "help model must name everyday verb `{verb}`: {model}"
            );
        }
        assert!(
            model.contains("heddle capture") && model.contains("heddle land"),
            "help model still teaches capture as the save and bare land: {model}"
        );
    }

    /// heddle#1458. The six field-walk residuals plus continue keep
    /// their everyday catalog labels.
    #[test]
    fn locked_collab_and_identity_verbs_are_everyday() {
        let catalog = crate::cli::commands::build_command_catalog();
        for verb in [
            "query", "review", "discuss", "context", "daemon", "whoami", "continue", "help",
        ] {
            let Some(entry) = catalog.command_by_display(verb) else {
                continue;
            };
            // Feature-gated roots (whoami without `client`) are advertised
            // with an empty summary and no live clap node. Skip them the
            // same way the first-screen renderer does.
            if entry.summary.is_empty() {
                continue;
            }
            assert_eq!(
                entry.help_visibility, "everyday",
                "`{verb}` catalog label must be everyday"
            );
        }
    }

    /// The catalog everyday set includes the locked first-screen verbs
    /// that have a contract entry in this build, plus leftover everyday
    /// labels that the 85→23 fold has not removed.
    #[test]
    fn everyday_verbs_surface_the_core_loop() {
        let everyday: std::collections::HashSet<&str> = everyday_verbs().into_iter().collect();
        for verb in [
            "init", "clone", "status", "start", "capture", "ready", "diff", "land", "resolve",
            "continue", "undo", "log", "show", "query", "review", "discuss", "context", "daemon",
            "pull", "push", "doctor", "help",
        ] {
            assert!(
                everyday.contains(verb),
                "`{verb}` is part of the locked everyday surface but is not \
                 advertised as everyday"
            );
        }
        if crate::cli::commands::build_command_catalog()
            .command_by_display("whoami")
            .is_some_and(|entry| !entry.summary.is_empty())
        {
            assert!(
                everyday.contains("whoami"),
                "`whoami` is part of the locked everyday surface"
            );
        }
        for verb in ["thread", "git-projection"] {
            assert!(
                !everyday.contains(verb),
                "`{verb}` is not an everyday first-screen verb"
            );
        }
    }

    /// heddle#278. The hidden agent-automation flags on `capture` need a
    /// discovery route: `heddle help agent-flags` must list them without
    /// presenting internal session transport variables as user configuration.
    #[test]
    fn agent_flags_topic_lists_hidden_capture_flags() {
        let text = topic_text("agent-flags").expect("agent-flags topic should exist");
        for flag in [
            "--agent-provider",
            "--agent-model",
            "--agent-session",
            "--agent-segment",
            "--policy",
            "--no-policy",
            "--no-agent",
            "--split",
            "--into",
            "--path",
        ] {
            assert!(text.contains(flag), "agent-flags topic missing `{flag}`");
        }
        for env in [
            "HEDDLE_AGENT_PROVIDER",
            "HEDDLE_AGENT_MODEL",
            "HEDDLE_AGENT_POLICY",
        ] {
            assert!(text.contains(env), "agent-flags topic missing env `{env}`");
        }
        for internal_env in ["HEDDLE_SESSION_ID", "HEDDLE_SESSION_SEGMENT"] {
            assert!(
                !text.contains(internal_env),
                "agent-flags topic must not advertise internal env `{internal_env}`"
            );
        }
    }

    #[test]
    fn git_help_distinguishes_overlay_from_projection_storage() {
        let overlay = topic_text("git-overlay").expect("git-overlay topic should exist");
        assert!(overlay.contains("neither creates nor reads `.heddle/git`"));

        let projection = topic_text("git-projection").expect("git-projection topic should exist");
        assert!(projection.contains("It is not Git Overlay"));
        assert!(projection.contains("heddle adopt --ref <branch>"));
        assert!(projection.contains("heddle bridge git export --destination"));
        assert!(projection.contains("Bridge Mirror"));
    }

    /// heddle#278. The agent flags stay `hide = true` in the default surface
    /// but `--help-agent` reveals every one of them.
    #[test]
    fn capture_agent_flags_hidden_by_default_revealed_on_demand() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let capture = cmd
            .find_subcommand("capture")
            .expect("capture subcommand exists");
        for id in CAPTURE_AGENT_FLAG_IDS {
            let arg = capture
                .get_arguments()
                .find(|arg| arg.get_id().as_str() == *id)
                .unwrap_or_else(|| panic!("capture has no `{id}` arg"));
            assert!(arg.is_hide_set(), "`{id}` should be hidden by default");
        }
        let revealed = reveal_capture_agent_flags(capture.clone());
        for id in CAPTURE_AGENT_FLAG_IDS {
            let arg = revealed
                .get_arguments()
                .find(|arg| arg.get_id().as_str() == *id)
                .expect("revealed arg present");
            assert!(
                !arg.is_hide_set(),
                "`{id}` should be revealed by --help-agent"
            );
        }
    }

    /// heddle#278 r4 (cid 3327325850). Close-the-class: `--help-agent` is a
    /// first-class clap flag on `capture`, so clap parses the whole command
    /// line and the dispatch arm inspects the parsed result. Whether the
    /// reveal help shows is exactly "did clap resolve `capture` with
    /// `--help-agent` set?" — no hand-rolled pre-parse verb scan, so every
    /// global spelling clap accepts is handled for free.
    ///
    /// `wants_reveal` mirrors the decision the `Commands::Capture` arm in
    /// `main.rs` makes (`matches!(cli.command, Commands::Capture(a) if
    /// a.help_agent)`), driven entirely by clap's parse.
    fn wants_reveal(args: &[&str]) -> bool {
        use clap::Parser;

        use crate::cli::cli_args::{Cli, Commands};
        let argv = std::iter::once("heddle").chain(args.iter().copied());
        match Cli::try_parse_from(argv) {
            Ok(cli) => matches!(&cli.command, Commands::Capture(a) if a.help_agent),
            // A parse error (e.g. `--help-agent` on a verb that has no such
            // flag) means: don't reveal — clap reports it as it would any
            // other invalid invocation.
            Err(_) => false,
        }
    }

    /// heddle#278. `--help-agent` reveals capture's agent flags only for the
    /// `capture` verb; plain `--help` and `--help-agent` on another verb do
    /// not.
    #[test]
    fn capture_help_agent_is_capture_scoped() {
        assert!(
            wants_reveal(&["capture", "--help-agent"]),
            "capture --help-agent should request the reveal help"
        );
        assert!(
            !wants_reveal(&["status", "--help-agent"]),
            "--help-agent on a non-capture verb is not a capture reveal request"
        );
        // `capture --help` is clap's own help; it exits during parse (a
        // DisplayHelp error), so it is never a `help_agent` reveal request.
        assert!(
            !wants_reveal(&["capture", "--help"]),
            "plain --help is clap's help, not the agent reveal"
        );
    }

    /// heddle#278 r2/r3/r4 (cids 3327112975 / 3327231819 / 3327325850).
    /// Because clap now owns parsing, every global spelling it accepts —
    /// long valued (`--output <fmt>`), short valued (`-C <path>`), and
    /// clustered short (`-vC <path>`) — is handled natively. With a repo dir
    /// literally named `capture`, the `-C` VALUE must not be read as the verb.
    /// This is the whole class the per-form pre-scan kept missing.
    #[test]
    fn capture_help_agent_handles_every_global_form_clap_accepts() {
        // Valued global before the verb: long and short, separated forms.
        assert!(
            wants_reveal(&["-C", "/tmp/repo", "capture", "--help-agent"]),
            "`-C <path> capture --help-agent` should reveal — clap parses the path"
        );
        assert!(
            wants_reveal(&["--output", "text", "capture", "--help-agent"]),
            "`--output text capture --help-agent` should reveal"
        );
        // Repo dir named `capture`: the `-C` value is `capture`, the verb is
        // the following `capture`.
        assert!(
            wants_reveal(&["-C", "capture", "capture", "--help-agent"]),
            "`-C capture capture --help-agent` (repo dir named `capture`) should reveal"
        );
        // r4: clustered short global `-vC <path>` — `-v` then valued `-C`.
        assert!(
            wants_reveal(&["-vC", "/tmp/repo", "capture", "--help-agent"]),
            "`-vC <path> capture --help-agent` (clustered short globals) should reveal"
        );
        assert!(
            wants_reveal(&["-vC", "capture", "capture", "--help-agent"]),
            "`-vC capture capture --help-agent` (clustered, repo dir named `capture`) should reveal"
        );
        // Attached short form `-C<path>`: value rides in the token.
        assert!(
            wants_reveal(&["-C/tmp/repo", "capture", "--help-agent"]),
            "`-C<path> capture --help-agent` (attached) should reveal"
        );
        // Plain invocation still reveals.
        assert!(
            wants_reveal(&["capture", "--help-agent"]),
            "plain `capture --help-agent` should reveal"
        );

        // Fall-through cases: the verb is NOT capture, so no reveal. With a
        // repo dir named `capture`, the `-C` value is consumed by clap and
        // `status` is the verb — `status` has no `--help-agent`, so clap
        // errors and we do not reveal.
        assert!(
            !wants_reveal(&["-C", "capture", "status", "--help-agent"]),
            "`-C capture status --help-agent` — verb is `status`, no reveal"
        );
        assert!(
            !wants_reveal(&["-vC", "capture", "status", "--help-agent"]),
            "`-vC capture status --help-agent` (clustered) — verb is `status`, no reveal"
        );
        assert!(
            !wants_reveal(&["--output", "text", "status", "--help-agent"]),
            "`--output text status --help-agent` — verb is `status`, no reveal"
        );
    }

    /// heddle#652. The `--output` machine-contract blurb (json vs
    /// json-compact fields, recovery templates) is stated in full exactly
    /// once on the top-level help — plus the dedicated `output-formats`
    /// topic — instead of being stamped by the global arg onto every
    /// command's --help. Per-command help carries only the one-line flag
    /// summary with the topic breadcrumb.
    #[test]
    fn output_blurb_stated_once_not_per_command() {
        let marker = "full machine contract";
        let top = render_for_args(&["--help"]).expect("top-level help renders");
        assert_eq!(
            top.matches(marker).count(),
            1,
            "top-level help should state the --output contract exactly once: {top}"
        );
        assert!(
            topic_text("output-formats")
                .expect("output-formats topic exists")
                .contains(marker),
            "the output-formats topic carries the full contract"
        );
        for argv in [
            &["clone", "--help"][..],
            &["status", "--help"][..],
            &["capture", "--help"][..],
            &["thread", "--help"][..],
            &["push", "--help"][..],
        ] {
            let help = render_for_args(argv).expect("command help renders");
            assert!(
                !help.contains(marker),
                "`{argv:?}` should not restate the --output machine contract: {help}"
            );
            assert!(
                help.contains("heddle help output-formats"),
                "`{argv:?}` should breadcrumb to the output-formats topic: {help}"
            );
        }
    }

    /// heddle#652. `clone --help` stays within one screen: signature,
    /// flags, a short Behavior summary, the hidden-flags breadcrumb
    /// (heddle#646), and examples. The full default-thread / --depth
    /// exposition lives in `heddle help clone`.
    #[test]
    fn clone_help_fits_one_screen() {
        let help = render_for_args(&["clone", "--help"]).expect("clone help renders");
        let lines = help.lines().count();
        assert!(
            lines <= 40,
            "clone --help should fit one screen (<= 40 lines), got {lines}:\n{help}"
        );
        // The trim must not cost discoverability: the hidden-flag
        // affordance and the deep-dive breadcrumb both survive.
        assert!(
            help.contains("Advanced/planned flags: see `heddle help clone`."),
            "clone --help keeps the hidden-flags affordance: {help}"
        );
        assert!(
            help.contains("heddle help clone"),
            "clone --help points at the clone topic for the full behavior: {help}"
        );
    }

    #[test]
    fn config_topic_documents_home_and_config_overrides() {
        let config = topic_text("config").expect("config topic exists");
        assert!(config.contains("HEDDLE_HOME"));
        assert!(config.contains("HEDDLE_CONFIG"));
        assert!(config.contains("<path>/config.toml"));
        assert!(config.contains("Principal resolution (highest first)"));
    }

    /// The ranked first screen is exhaustive: every non-hidden root in
    /// the contract table renders (unless feature-gated out of this
    /// build), and the locked everyday head keeps its rank order ahead
    /// of the ranked tail.
    #[test]
    fn first_screen_ranks_locked_head_then_every_remaining_root() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let catalog = crate::cli::commands::build_command_catalog();
        let help = render_help_for_authority(&cmd, &[], repo::RepositorySourceAuthority::Native);

        let listed: Vec<&str> = help
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim_start();
                let name = trimmed.split_whitespace().next()?;
                catalog.command_by_display(name).is_some().then_some(name)
            })
            .collect();

        for entry in &catalog.commands {
            if entry.path.len() != 1
                || entry.help_visibility == "hidden"
                || entry.summary.is_empty()
            {
                continue;
            }
            let display: &str = &entry.display;
            assert!(
                listed.contains(&display),
                "non-hidden root `{display}` must render on `heddle help`: {help}"
            );
        }

        // Head-of-list: the LAST locked verb's rank must not exceed any
        // tail verb rendered after it... simpler invariant: every locked
        // verb appears before every non-locked one.
        let positions: Vec<usize> = LOCKED_EVERYDAY_VERBS
            .iter()
            .filter_map(|verb| listed.iter().position(|name| name == verb))
            .collect();
        if let (Some(first_tail), Some(last_head)) = (
            listed
                .iter()
                .position(|name| !LOCKED_EVERYDAY_VERBS.contains(name)),
            positions.last(),
        ) {
            assert!(
                *last_head < first_tail,
                "locked everyday head must precede the ranked tail: {listed:?}"
            );
        }
    }

    /// Build-break property: every verb on the ranked screen that's
    /// compiled into the current build MUST resolve to a command catalog
    /// entry with a non-empty summary. Verbs gated behind a feature that
    /// isn't enabled (e.g. `semantic` when the `semantic` feature is off)
    /// are skipped — `print_help` already skips them at render time. If a
    /// verb is renamed in the contract table without a matching catalog
    /// entry, this test fails for whichever feature combo the variant
    /// lives in.
    #[test]
    fn verb_blurbs_resolve_from_command_catalog() {
        let catalog = crate::cli::commands::build_command_catalog();
        for verb in crate::cli::commands::ranked_visible_roots() {
            // Feature-gated verbs may not be present in this build —
            // skip them. The render path mirrors this.
            if catalog.command_by_display(verb).is_none() {
                continue;
            }
            let blurb = catalog_summary(&catalog, verb);
            assert!(
                !blurb.is_empty(),
                "verb `{verb}` is cataloged but its summary is empty. \
                 The curated help printer needs a non-empty catalog summary."
            );
        }
    }

    /// heddle#646. Close-the-class: every `hide = true` flag on a visible
    /// command must carry a discovery affordance, so no flag is learnable
    /// only by reading the source (the clone `--lazy`/`--filter` gap).
    /// Two affordances are recognized:
    ///
    /// (a) internal plumbing — the flag's own help text starts with
    ///     "Internal", declaring it not-for-users (test helpers, hints
    ///     automation sets on the user's behalf); or
    /// (b) a help breadcrumb — the command's after-help carries an
    ///     advanced/hidden-flags note (mentions "hidden" or "advanced
    ///     flag") that either names the flag (`--<long>`) inline or
    ///     points at a reveal surface (`heddle help <topic>` /
    ///     `--help-agent`).
    ///
    /// Hidden commands (debug surfaces like `index`/`monitor`) are
    /// skipped wholesale — their entire surface is intentionally
    /// unadvertised. Global args are skipped (`--op-id` has its own
    /// contract-driven reveal in `help_command_for_path` plus the
    /// `operation-ids` topic).
    #[test]
    fn hidden_flags_carry_discovery_affordances() {
        use clap::CommandFactory;

        fn walk(cmd: &clap::Command, path: &str, violations: &mut Vec<String>) {
            let after_help = [
                cmd.get_after_help().map(ToString::to_string),
                cmd.get_after_long_help().map(ToString::to_string),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
            let after_lower = after_help.to_lowercase();
            let breadcrumb_marker = after_lower.contains("hidden")
                || after_lower.contains("advanced flag")
                || after_lower.contains("advanced/planned flags");
            let points_at_reveal =
                after_help.contains("heddle help ") || after_help.contains("--help-agent");

            for arg in cmd.get_arguments() {
                if !arg.is_hide_set() || arg.is_global_set() {
                    continue;
                }
                let flag_help = arg
                    .get_long_help()
                    .or_else(|| arg.get_help())
                    .map(ToString::to_string)
                    .unwrap_or_default();
                if flag_help.starts_with("Internal") {
                    continue;
                }
                let named_inline = arg
                    .get_long()
                    .is_some_and(|long| after_help.contains(&format!("--{long}")));
                if breadcrumb_marker && (points_at_reveal || named_inline) {
                    continue;
                }
                let name = arg
                    .get_long()
                    .map(|long| format!("--{long}"))
                    .unwrap_or_else(|| arg.get_id().to_string());
                violations.push(format!("`{path}` hides `{name}`"));
            }

            for sub in cmd.get_subcommands() {
                if sub.is_hide_set() {
                    continue;
                }
                walk(sub, &format!("{path} {}", sub.get_name()), violations);
            }
        }

        let cmd = crate::cli::cli_args::Cli::command();
        let mut violations = Vec::new();
        walk(&cmd, cmd.get_name(), &mut violations);
        assert!(
            violations.is_empty(),
            "hidden flags without a discovery affordance (heddle#646): either \
             prefix the flag's help with `Internal` (plumbing, not for users) \
             or add an after-help breadcrumb that mentions the hidden/advanced \
             flags and names the flag or a reveal surface:\n  {}",
            violations.join("\n  ")
        );
    }
}
