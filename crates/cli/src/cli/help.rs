// SPDX-License-Identifier: Apache-2.0
//! Progressive-disclosure help: curated default, advanced surface,
//! topic-scoped help.
//!
//! The Heddle CLI's default `heddle help` lists only the native loop
//! from the command contract table. Advanced affordances, automation,
//! admin commands, and Git adapter commands are reachable
//! via `heddle help advanced` or `heddle help <topic>`. Per-verb help
//! via `heddle <verb> --help` continues to derive from clap
//! doc-comments.
//!
//! # Cultural deliverable
//!
//! The default help is **curated, not auto-generated**. Tier metadata
//! lives in the command contract table so human help and machine
//! command catalog output cannot drift. See `AGENTS.md` "CLI surface
//! curation" for the full doctrine.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Front-door verbs in the core loop: setup, orient, isolate work,
    /// commit, check readiness, inspect, integrate, recover, and
    /// diagnose. See [`everyday_verbs`] for the authoritative list.
    Everyday,
    /// Reachable via `heddle help advanced` or `heddle help <topic>`.
    /// Most agent-loop and operational verbs land here.
    Advanced,
    /// Hidden verbs that should not be advertised at all.
    Hidden,
}

/// Stable name for each verb. Pure presentation — has no relation to the
/// clap variant identifier so doc-comment regenerations don't churn this
/// file.
pub fn tier_of(verb: &str) -> Tier {
    match crate::cli::commands::command_help_tier(verb) {
        "everyday" => Tier::Everyday,
        "hidden" => Tier::Hidden,
        _ => Tier::Advanced,
    }
}

/// Verbs that show in `heddle help`, in command-contract order. Blurbs
/// are looked up at print time from the command catalog.
pub fn everyday_verbs() -> Vec<&'static str> {
    crate::cli::commands::root_commands_for_help_visibility("everyday")
}

/// The first screen of help is smaller than the complete everyday set:
/// it shows the primary work loop, then points at setup, sync, proof,
/// and recovery as nearby verbs.
fn primary_loop_verbs(catalog: &crate::cli::commands::CommandCatalogOutput) -> Vec<&'static str> {
    everyday_verbs()
        .into_iter()
        .filter(|verb| {
            catalog
                .command_by_display(verb)
                .is_some_and(|entry| entry.help_rank <= 70)
        })
        .collect()
}

/// Verbs surfaced by `heddle help advanced`, in command-contract order.
/// This includes power surfaces, automation/admin commands, and
/// Git-shaped aliases, each labeled by the contract table.
pub fn advanced_verbs() -> Vec<&'static str> {
    crate::cli::commands::root_commands_for_advanced_help()
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
/// AND the bare-help intercept in `main.rs`. Routes between everyday
/// / advanced / topic surfaces and falls through to clap-derived help
/// for command paths without a dedicated topic.
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
    use std::fmt::Write;
    let mut out = String::new();
    match topic {
        [] => {
            let catalog = crate::cli::commands::build_command_catalog();
            let _ = writeln!(out, "Heddle — AI-native version control");
            let _ = writeln!(out);
            let _ = writeln!(out, "Common loop:");
            for name in primary_loop_verbs(&catalog) {
                let blurb = catalog_summary(&catalog, name);
                if blurb.is_empty() {
                    continue;
                }
                let _ = writeln!(out, "  {:<10}  {}", name, blurb);
            }
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "Existing Git: heddle status -> heddle adopt -> heddle verify -> heddle commit -m \"...\" -> heddle push"
            );
            let _ = writeln!(
                out,
                "Isolated work: heddle start <name> --path ../<name> -> heddle commit -m \"...\" -> heddle ready -> heddle land"
            );
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "Nearby: `heddle undo`, `heddle verify`, `heddle push`, `heddle pull`."
            );
            let _ = writeln!(
                out,
                "Start here: `heddle init`, `heddle adopt`, or `heddle clone`."
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
                 `heddle help advanced` for power surfaces, automation, and Git interop, \
                 or `heddle help <topic>` for a topic page (e.g. `git-overlay`, \
                 `threads`, `daemon`, `signals`, `bridge`, `operation-ids`, \
                 `remotes`, `output-formats`, `git-dependencies`)."
            );
        }
        [name] if name == "advanced" => {
            let catalog = crate::cli::commands::build_command_catalog();
            let _ = writeln!(out, "{}", ADVANCED_HELP);
            // Grouped by area from the command contract table — the
            // group is registration data (`help_category` / surface),
            // not a hand-maintained help string (heddle#652). The group
            // header replaces the old per-line `[advanced]`-style
            // surface label; only the `use <canonical>` redirect for
            // Git-shaped aliases stays on the line.
            for (title, verbs) in crate::cli::commands::advanced_help_groups() {
                let mut lines = Vec::new();
                for name in verbs {
                    let blurb = catalog_summary(&catalog, name);
                    if blurb.is_empty() {
                        continue;
                    }
                    let canonical = crate::cli::commands::command_canonical_command(name)
                        .map(|canonical| format!(" [use `{canonical}`]"))
                        .unwrap_or_default();
                    lines.push(format!("  {name:<14}  {blurb}{canonical}"));
                }
                if lines.is_empty() {
                    continue;
                }
                let _ = writeln!(out, "{title}:");
                for line in lines {
                    let _ = writeln!(out, "{line}");
                }
                let _ = writeln!(out);
            }
        }
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
                    "no topic or command '{name}'. Run `heddle help advanced` for \
                     the full advanced list, or `heddle help` for the \
                     curated everyday surface."
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
            arg.get_long().is_some_and(|long| token == format!("--{long}"))
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
/// mechanism differs (HeddleCo/heddle#381). Help is repo-, cwd-, and
/// env-independent, so this is safe to call from parallel tests.
pub fn render_for_args(args: &[&str]) -> Option<String> {
    use crate::cli::cli_args::{Cli, Commands};
    use clap::{CommandFactory, Parser};

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
        // `heddle help <topics>` — curated topic / advanced / command-path help.
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
        "advanced" => ADVANCED_HELP,
        "agent-flags" => AGENT_FLAGS_TOPIC,
        "agent" | "daemon" => DAEMON_TOPIC,
        "output-formats" | "output-format" | "output" => OUTPUT_FORMATS_TOPIC,
        "clone" => CLONE_TOPIC,
        "git-overlay" => GIT_OVERLAY_TOPIC,
        "model" | "mental-model" | "concepts" => MODEL_TOPIC,
        "threads" => THREADS_TOPIC,
        "operation-ids" | "idempotency" => OPERATION_IDS_TOPIC,
        "remotes" => REMOTES_TOPIC,
        "git-dependencies" | "git-deps" | "git-dependency" => GIT_DEPENDENCIES_TOPIC,
        "review" => REVIEW_TOPIC,
        "discuss" | "discussions" => DISCUSS_TOPIC,
        "bridge" | "footer" | "notes" => BRIDGE_TOPIC,
        "signals" | "risk-signals" => SIGNALS_TOPIC,
        _ => return None,
    })
}

const ADVANCED_HELP: &str = "Advanced commands for power users, agents, automation, Git interop, and recovery.\n\
\n\
The default `heddle help` curates the native loop: init/adopt/clone,\n\
status/diff/commit/start, ready/land/push/pull, resolve/continue/abort,\n\
doctor/verify. Power nouns such as thread/workspace/remote/bridge/agent and\n\
Git adapter commands live behind this topic. Use `heddle help\n\
<verb>` for curated topics or `heddle <verb> --help` for the full clap-derived\n\
docs.\n\
\n\
This is intentional. The everyday surface stays minimal so first-time users aren't\n\
overwhelmed; agents and power users reach for the advanced affordances when they\n\
need them.\n";

// The single full statement of the --output machine contract (plus the
// one-paragraph version on the top-level `heddle help`). The global
// `--output` flag's own help is one line pointing here, so the contract
// is not restated on every command's --help (heddle#652).
const OUTPUT_FORMATS_TOPIC: &str = r#"Output formats — `--output text | json | json-compact`.

`text` is the default, always. There is no TTY/pipe auto-detection — the
default never switches under you, so scripts and humans see the same thing
until a flag says otherwise.

`--output json` emits the full machine contract: a stable `output_kind`
discriminator, exit codes, and recovery templates. Schemas per verb:
`heddle schemas <verb>`; the catalog of which commands emit what:
`heddle commands --output json`.

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
const CLONE_TOPIC: &str = r#"Cloning — Git repositories and Heddle remotes.

    heddle clone <remote> <dir> [--thread <name>] [--depth <n>]

Run `heddle clone --help` for the flag list.

# Which thread the clone lands on (no --thread)

- Git-overlay clones (cloning a Git repository) land on the remote's
  advertised default branch (its Git HEAD); if the remote advertises
  none, they fall back to a thread named `main`, then to the
  alphabetically first imported thread.
- Native-local and hosted Heddle clones target `main` directly with no
  fallback chain; if the remote has no `main` thread the clone fails —
  pass `--thread <name>` to select one.
- Clone never prompts.

# Shallow clones (--depth, Heddle remotes only)

--depth 0 (the default) clones full history. --depth N fetches only the
tip plus N generations of ancestry (--depth 1: the tip plus its immediate parents),
so `heddle log` stops at the depth boundary; history older than that is
not present locally — re-clone at a greater --depth (or --depth 0) to
obtain it. Git-overlay clones reject a nonzero --depth; --depth 0 is accepted
and clones full history.

Depth controls history extent only — how many states the clone fetches —
and says nothing about object contents. Whether a state's blobs are
present locally or fetched lazily is a separate concern that `--depth`
never governs (see the hidden `--lazy` / `--filter blob:none` flags in
`heddle clone --help`; hosted/network Heddle remotes only).

See `heddle help threads` for the thread model and `heddle help remotes`
for remote management.
"#;

const AGENT_FLAGS_TOPIC: &str = r#"Agent automation flags for `heddle capture`.

These flags are hidden from the everyday `heddle capture --help` so it stays
terse for human use. They let an automated caller override agent attribution
and split captures across threads. Run `heddle capture --help-agent` to see
them inline in capture's own help.

Attribution overrides (each falls back to the matching env var, then config):

  --agent-provider <NAME>   Override HEDDLE_AGENT_PROVIDER.
  --agent-model <NAME>      Override HEDDLE_AGENT_MODEL.
  --agent-session <ID>      Override the active agent session id (HEDDLE_SESSION_ID).
  --agent-segment <ID>      Override the active session segment (HEDDLE_SESSION_SEGMENT).
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
env var, harness probe, active session, user config, repo config. See
`crates/cli/src/cli/commands/snapshot.rs` for the full cascade.
"#;

const DAEMON_TOPIC: &str = "Two daemons — both have legitimate uses; they are not interchangeable.\n\
\n\
`heddle daemon`        — FUSE mount-daemon control plane. Owns FUSE sessions for\n\
                         `--workspace virtualized --daemon` threads. Linux only.\n\
                         Subcommands: serve | status | stop.\n\
\n\
`heddle agent serve`   — Local gRPC daemon over a Unix socket inside the repo's\n\
                         `.heddle/sockets/`. Hosts the local agent\n\
                         services (state-review, discussion, signal, operation-log\n\
                         query, hook) so agents avoid per-command\n\
                         process startup latency. Mode: same-user only;\n\
                         peer-credential checks are enforced.\n";

const MODEL_TOPIC: &str = r#"Heddle mental model — the everyday loop in one screen.

Heddle is built around saved states and isolated threads. Git compatibility is
an output and interop layer, not the thing you have to think about first.

Core nouns:

- State: a captured tree with a stable change id, attribution, intent, and
  provenance. States are what `log`, `show`, `diff`, `undo`, and agents can
  reason about.
- Thread: a named line of work with its own checkout and captured history.
  Use it for risky edits, agent work, or parallel experiments without stash
  juggling.
- Capture: a cheap recoverable save point on the current thread.
- Commit: the normal human save path. In native Heddle it saves the state; in a
  Git-overlay repo it saves the Heddle state and writes the matching Git
  checkpoint as one operation.
- Checkpoint: the explicit Git-overlay boundary for already-captured work.
- Verify: the proof surface. It says whether Heddle, Git mapping, worktree,
  remotes, active operations, clone state, and machine contracts agree.

Everyday loop:

    heddle status
    heddle diff
    heddle commit -m "..."
    heddle start <name> --path ../<name>
    heddle ready
    heddle land --thread <name>
    heddle undo
    heddle verify

Existing Git checkout:

    heddle status
    heddle adopt                 # or the exact adopt/import command status prints
    heddle verify

If a command refuses, read the first `Next:` line. Heddle fails closed when it
cannot prove the move is safe.
"#;

const THREADS_TOPIC: &str = "Threads — Heddle's unit of in-progress work.\n\
\n\
A thread is a named line of work with its own checkout, its own captured\n\
history, and a target it eventually merges into. It is *not* a git branch:\n\
the git-overlay branch is downstream plumbing (created at checkpoint),\n\
not the primary object. You start work with `heddle start <name>`, switch\n\
between threads with `heddle thread switch <name>`, and integrate with\n\
`heddle land` (or check readiness without merging via `heddle ready`).\n\
\n\
# Threads vs. git branches\n\
\n\
- A thread carries an isolated checkout (its own directory), captured\n\
  state history, agent/task metadata, a freshness verdict against its\n\
  target, and a workflow state (Ready/Blocked/Merged/...). A git branch\n\
  is just a ref.\n\
- Multiple threads coexist on disk simultaneously without `git stash` /\n\
  `git worktree` gymnastics. Each thread's working tree is its own.\n\
- `heddle commit` captures work and writes the Git-facing checkpoint.\n\
  Use `heddle capture` and `heddle checkpoint` separately when you want\n\
  finer-grained Heddle states before producing Git commits.\n\
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
the workflow's point of view — `capture`, `land`, `switch`, etc. behave\n\
identically. The mode only controls bytes-on-disk semantics.\n\
\n\
# Materialize vs. promote\n\
\n\
- Choose a workspace mode at `heddle start` time: pass `--workspace\n\
  materialized` (or rely on `auto`) when you want real bytes on disk\n\
  from the start.\n\
- `heddle thread promote <name> --path <dir>` upgrades an existing\n\
  thread to an isolated materialized checkout at a chosen path. Use it when a thread\n\
  that started lightweight (`virtualized`, or no on-disk checkout)\n\
  needs to become a real working tree — for example, to hand it to a\n\
  tool that can't read through the mount.\n\
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
# `switch` vs. `git checkout` vs. `thread switch`\n\
\n\
These three look similar but operate at different layers:\n\
\n\
- `heddle thread switch <name>` — change which *thread* is active.\n\
  Each thread has its own checkout; switching may auto-capture\n\
  outstanding work on the thread you're leaving. Pair with the shell\n\
  hook (`heddle shell init`) to auto-cd into the target thread's\n\
  directory.\n\
- `heddle switch <state>` — move the *current thread's* worktree to a\n\
  specific captured state. It refuses with uncommitted changes unless\n\
  `--force` is passed; it does not change which thread is active.\n\
- `git checkout` — operates on the git-overlay branch and index\n\
  directly. Heddle's thread metadata, captured state, and workflow\n\
  state are not updated. Reach for it only when you specifically want\n\
  the git-layer view; the thread-aware verbs are the supported path.\n\
\n\
# Capture vs. checkpoint\n\
\n\
- `heddle capture` records a recoverable Heddle step on the current\n\
  thread — for undo, provenance, and review. Captures are\n\
  fine-grained and accumulate freely as work progresses.\n\
- `heddle commit -m \"...\"` is the one-step human path: capture the\n\
  current work and write the Git-facing checkpoint.\n\
- `heddle checkpoint` commits the current captured work to the\n\
  git-overlay branch/index. It refuses when the worktree has changes\n\
  that haven't been captured yet — capture first, then checkpoint.\n\
- The split lets agents and tools take many small captures (cheap,\n\
  reversible) without producing a noisy git history; checkpoints are\n\
  the durable downstream record.\n\
\n\
See also: `heddle help advanced` for the full operational surface,\n\
`heddle thread --help` for the thread subcommand list.\n";

const OPERATION_IDS_TOPIC: &str = "Idempotency — machine retries for supported mutating commands.\n\
\n\
Commands that advertise `supports_op_id: true` in `heddle commands --output json`\n\
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
authoritative per-command contract, use `heddle commands --output json`.\n";

const REMOTES_TOPIC: &str = "Remotes — local, Git-overlay, and hosted destinations.\n\
\n\
Core loop:\n\
\n\
    heddle remote add origin <url-or-path>\n\
    heddle remote set-default origin\n\
    heddle fetch\n\
    heddle push\n\
    heddle pull\n\
    heddle verify\n\
\n\
Remote values may be hosted endpoints, Git URLs, file URLs, or local bare Git\n\
paths depending on the workflow. Top-level `fetch`, `push`, and `pull` use the\n\
default remote unless a positional remote name is supplied, for example\n\
`heddle fetch backup`. `heddle bridge git status` shows Git-overlay mapping and\n\
drift before a sync operation changes refs.\n\
\n\
When a remote action is unsafe, Heddle reports the blocker and one primary\n\
next command instead of falling back to raw Git.\n";

const GIT_DEPENDENCIES_TOPIC: &str = "Git executable dependencies — what works without `git` on PATH.\n\
\n\
Supported Git-overlay workflows use native/library paths and are tested with\n\
`PATH` stripped of `git`: `init`, `status`, local/bare `clone`, `bridge git\n\
import`, `bridge git status`, `bridge git sync/export` where implemented,\n\
`thread list`, `workspace`, `log`, `show`, `diff`, `checkpoint`, `merge`,\n\
`ready`, and `fsck`.\n\
\n\
Heddle is Git-compatible, not Git-binary-dependent. Public CLI runtime paths\n\
must not spawn a `git` process; Git-format reads, writes, transport, index,\n\
and ref updates go through native/library code.\n\
\n\
If Heddle detects an externally-started raw Git sequencer operation, it leaves\n\
Git metadata, refs, index, and worktree files unchanged and reports a Heddle\n\
preservation command. Finish or abort that operation with the Git-compatible\n\
tool that started it, then run `heddle verify`.\n\
\n\
Unsupported native Git-overlay capabilities, such as filtered/lazy Git clones,\n\
fail closed with recovery advice instead of silently invoking a `git` binary.\n\
`merge --git-commit` writes Git objects and refs natively.\n\
\n\
Run `heddle commands --output json` to inspect the public command surface, and\n\
`heddle doctor` / `heddle fsck --full` when a repository reports integrity or\n\
bridge-state problems.\n";

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

const DISCUSS_TOPIC: &str = "`heddle discuss open | append | resolve | list | show`\n\
\n\
Discussions anchor at the symbol level (file + symbol name, no line range)\n\
so they survive renames and cross-file moves. Each discussion accumulates\n\
turns and resolves into one of three terminal states:\n\
\n\
- `resolve <id> --mode into-annotation`  with `--annotation-kind`,\n\
  `--annotation-content`, optional `--annotation-tags`. Atomically\n\
  creates the annotation and bidirectionally links it.\n\
- `resolve <id> --mode by-edit`          with `--state` (defaults to HEAD).\n\
  Records that a subsequent edit addressed the discussion.\n\
- `resolve <id> --mode dismiss`          requires non-empty `--reason`.\n\
\n\
Visibility: `--visibility public|internal|team:NAME|restricted:LABEL`.\n\
Defaults to the repo's namespace policy.\n";

const GIT_OVERLAY_TOPIC: &str = "Git-overlay quick start\n\
\n\
Use this when you want Heddle's captured states, isolated threads, merge\n\
previews, undo, provenance, and machine-safe JSON with Git compatibility kept\n\
behind the bridge/adapter.\n\
\n\
Start in an existing Git checkout:\n\
\n\
    heddle status\n\
    heddle adopt --ref <branch>               # use the exact command printed by status\n\
    heddle verify\n\
\n\
Save and sync ordinary work:\n\
\n\
    heddle diff\n\
    heddle commit -m \"...\"                    # one Heddle state + one Git commit\n\
    heddle push\n\
\n\
Isolate risky work:\n\
\n\
    heddle start <name> --path ../<name>\n\
    cd ../<name>\n\
    heddle commit -m \"...\"\n\
    heddle ready\n\
    cd -\n\
    heddle land --thread <name> --no-push     # add --push when ready to publish\n\
\n\
Recover or prove state:\n\
\n\
    heddle undo\n\
    heddle verify\n\
\n\
State-specific recovery:\n\
\n\
    Worktree has unsaved edits: heddle commit -m \"...\"\n\
    Captured in Heddle but not Git: heddle commit -m \"...\"\n\
    Git refs changed externally: heddle adopt --ref <branch>\n";

const BRIDGE_TOPIC: &str = "Git bridge — adopt existing Git repos through an adapter.\n\
\n\
Use the bridge when you are standing in a normal Git checkout and want Heddle's\n\
captured states, isolated threads, merge previews, undo, and machine-safe JSON\n\
while keeping Git remotes and commits available as interoperability surfaces.\n\
\n\
First run:\n\
\n\
    heddle status\n\
    heddle adopt --ref <branch>               # use the exact command printed by status\n\
    heddle verify\n\
\n\
Manual setup, when you want one ref at a time:\n\
\n\
    heddle init\n\
    heddle bridge git import --ref <branch>\n\
\n\
Daily loop:\n\
\n\
    heddle status\n\
    heddle commit -m \"...\"                    # save work as Heddle + Git\n\
    heddle push                               # Git-overlay remotes use the top-level verb\n\
    heddle start <name> --path ../<name>\n\
    heddle ready --thread <name>              # or cd into ../<name> and run heddle ready\n\
    heddle land --thread <name> --no-push     # add --push to land and push together\n\
\n\
Recovery and inspection:\n\
\n\
    heddle bridge git status\n\
    heddle bridge git reconcile --ref <branch> --preview\n\
    heddle doctor\n\
    heddle verify --output json\n\
\n\
Export metadata for Git readers:\n\
\n\
Every exported commit carries a footer at the tail of the commit message:\n\
\n\
    Heddle-State: <change_id>\n\
    Heddle-URL: <hosted_url>/state/<change_id>     (omitted if no hosted URL)\n\
    Heddle-Annotations-Omitted: <count>\n\
\n\
This is the durable record — every reader on every host sees it regardless\n\
of remote configuration.\n\
\n\
Per-scope annotation drop counts and signal counts ride on the opt-in\n\
Git note at `refs/notes/heddle`. Heddle reads and writes that ref natively;\n\
people who still inspect the repository through another Git client can opt\n\
that client into showing notes, but Heddle itself does not require a Git\n\
executable on the system.\n";

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
    fn everyday_verbs_in_curated_list_have_everyday_tier() {
        for verb in everyday_verbs() {
            assert_eq!(tier_of(verb), Tier::Everyday, "{verb}");
        }
    }

    #[test]
    fn topic_text_returns_none_for_unknown() {
        assert!(topic_text("definitely-not-a-topic").is_none());
    }

    #[test]
    fn topic_text_returns_some_for_advertised_topics() {
        for topic in [
            "advanced",
            "agent-flags",
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
            "review",
            "discuss",
            "discussions",
            "bridge",
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

    #[test]
    fn tier_of_advanced_verbs_classifies_correctly() {
        for verb in advanced_verbs() {
            let t = tier_of(verb);
            assert!(
                matches!(t, Tier::Advanced),
                "expected Advanced for {verb}, got {t:?}"
            );
        }
    }

    /// Regression: heddle#150. `query`, `capture`, `checkpoint`, `continue`, and
    /// `abort` are referenced in inline tips and error messages but
    /// were absent from the `heddle help advanced` listing, leaving
    /// users unable to discover the verb they were told to run.
    #[test]
    fn advanced_verbs_lists_tip_referenced_commands() {
        let advanced: std::collections::HashSet<&str> = advanced_verbs().into_iter().collect();
        for verb in [
            "query",
            "capture",
            "checkpoint",
            "continue",
            "abort",
            "shell",
            "git-overlay",
        ] {
            assert!(
                advanced.contains(verb),
                "`{verb}` is referenced in user-facing tips but is not \
                 advertised by `heddle help advanced`"
            );
        }
    }

    /// The everyday surface should mirror the core loop rather than
    /// mixing in every collaboration feature. A first-time user should
    /// be able to orient, commit work, check readiness, inspect,
    /// integrate, recover, and diagnose from the first help screen.
    #[test]
    fn everyday_verbs_surface_the_core_loop() {
        let everyday: std::collections::HashSet<&str> = everyday_verbs().into_iter().collect();
        for verb in [
            "init", "clone", "status", "start", "commit", "ready", "diff", "land",
            "resolve", "undo", "log", "show", "pull", "push", "doctor", "verify",
        ] {
            assert!(
                everyday.contains(verb),
                "`{verb}` is part of the core loop but is not advertised on \
                 the everyday surface"
            );
        }
        for verb in ["review", "discuss", "context", "thread", "bridge"] {
            assert!(
                !everyday.contains(verb),
                "`{verb}` belongs behind advanced/topic help, not the core-loop surface"
            );
        }
    }

    /// heddle#278. The hidden agent-automation flags on `capture` need a
    /// discovery route: `heddle help agent-flags` must list them with their
    /// env equivalents.
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
            "HEDDLE_SESSION_ID",
            "HEDDLE_SESSION_SEGMENT",
        ] {
            assert!(text.contains(env), "agent-flags topic missing env `{env}`");
        }
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
            assert!(!arg.is_hide_set(), "`{id}` should be revealed by --help-agent");
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
        use crate::cli::cli_args::{Cli, Commands};
        use clap::Parser;
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
            &["commit", "--help"][..],
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
            help.contains("Advanced (hidden) flags:"),
            "clone --help keeps the hidden-flags affordance: {help}"
        );
        assert!(
            help.contains("heddle help clone"),
            "clone --help points at the clone topic for the full behavior: {help}"
        );
    }

    /// heddle#652. `heddle help advanced` renders area groups instead of
    /// one flat alphabetical wall of commands.
    #[test]
    fn advanced_help_renders_area_groups() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let advanced = render_help(&cmd, &["advanced".to_string()]);
        for header in [
            "Threads and integration:",
            "States and history:",
            "Recovery and integrity:",
            "Repo and environment:",
            "Agents and automation:",
            "Git interop:",
            "Admin and maintenance:",
        ] {
            assert!(
                advanced.contains(&format!("\n{header}\n")),
                "advanced help should render the `{header}` group: {advanced}"
            );
        }
        assert!(
            !advanced.contains("Advanced commands:"),
            "the flat list header is replaced by area groups: {advanced}"
        );
    }

    /// heddle#652. The grouped advanced surface is exhaustive and
    /// non-overlapping: every advanced verb appears in exactly one group.
    /// Because native commands group by the `help_category` on their
    /// contract registration, a new advanced native root command that
    /// forgets to pick a category fails here instead of silently
    /// vanishing from `heddle help advanced`.
    #[test]
    fn advanced_help_groups_cover_every_advanced_verb() {
        let grouped: Vec<&str> = crate::cli::commands::advanced_help_groups()
            .into_iter()
            .flat_map(|(_, verbs)| verbs)
            .collect();
        let mut deduped = grouped.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            grouped.len(),
            "no verb may appear in two advanced-help groups: {grouped:?}"
        );
        let grouped: std::collections::HashSet<&str> = grouped.into_iter().collect();
        let flat: std::collections::HashSet<&str> = advanced_verbs().into_iter().collect();
        let missing: Vec<&&str> = flat.difference(&grouped).collect();
        assert!(
            missing.is_empty(),
            "advanced verbs missing from every group — native advanced root \
             commands must register a help_category in the command contract \
             table: {missing:?}"
        );
        let extra: Vec<&&str> = grouped.difference(&flat).collect();
        assert!(
            extra.is_empty(),
            "grouped verbs not on the advanced surface: {extra:?}"
        );
    }

    /// Build-break property: every verb listed in `everyday_verbs` and
    /// `advanced_verbs` that's compiled into the current build MUST
    /// resolve to a command catalog entry with a non-empty summary. Verbs
    /// gated behind a feature that isn't enabled (e.g. `semantic` when
    /// the `semantic` feature is off) are skipped — `print_help`
    /// already skips them at render time. If a verb is renamed in the
    /// contract table without a matching catalog entry, this test fails
    /// for whichever feature combo the variant lives in.
    #[test]
    fn verb_blurbs_resolve_from_command_catalog() {
        let catalog = crate::cli::commands::build_command_catalog();
        for verb in everyday_verbs().into_iter().chain(advanced_verbs()) {
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
            let breadcrumb_marker =
                after_lower.contains("hidden") || after_lower.contains("advanced flag");
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
