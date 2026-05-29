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
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match topic {
        [] => {
            let catalog = crate::cli::commands::build_command_catalog();
            writeln!(out, "Heddle — AI-native version control")?;
            writeln!(out)?;
            writeln!(out, "Common loop:")?;
            for name in primary_loop_verbs(&catalog) {
                let blurb = catalog_summary(&catalog, name);
                if blurb.is_empty() {
                    continue;
                }
                writeln!(out, "  {:<10}  {}", name, blurb)?;
            }
            writeln!(out)?;
            writeln!(
                out,
                "Existing Git: heddle status -> heddle adopt -> heddle verify -> heddle commit -m \"...\" -> heddle push"
            )?;
            writeln!(
                out,
                "Isolated work: heddle start <name> --path ../<name> -> heddle ready -> heddle merge --preview -> heddle ship"
            )?;
            writeln!(out)?;
            writeln!(
                out,
                "Nearby: `heddle undo`, `heddle verify`, `heddle push`, `heddle pull`."
            )?;
            writeln!(
                out,
                "Start here: `heddle init`, `heddle adopt`, or `heddle clone`."
            )?;
            writeln!(out)?;
            writeln!(
                out,
                "Output: text is the default; pass `--output json` for the \
                 machine contract (stable `output_kind`, exit codes, recovery \
                 templates). No TTY/pipe auto-detection."
            )?;
            writeln!(out)?;
            writeln!(
                out,
                "Run `heddle help model` for the short mental model, \
                 `heddle help advanced` for power surfaces, automation, and Git interop, \
                 or `heddle help <topic>` for a topic page (e.g. `git-overlay`, \
                 `threads`, `daemon`, `signals`, `bridge`, `operation-ids`, \
                 `remotes`, `git-dependencies`)."
            )?;
        }
        [name] if name == "advanced" => {
            let catalog = crate::cli::commands::build_command_catalog();
            writeln!(out, "{}", ADVANCED_HELP)?;
            writeln!(out, "Advanced commands:")?;
            for name in advanced_verbs() {
                let blurb = catalog_summary(&catalog, name);
                if blurb.is_empty() {
                    continue;
                }
                let visibility = crate::cli::commands::command_help_visibility(name);
                let surface = crate::cli::commands::command_surface(name);
                let label = if visibility == "git_adapter" || surface != "native" {
                    surface
                } else {
                    visibility
                };
                let canonical = crate::cli::commands::command_canonical_command(name)
                    .map(|canonical| format!("; use `{canonical}`"))
                    .unwrap_or_default();
                writeln!(out, "  {:<14}  {} [{}{}]", name, blurb, label, canonical)?;
            }
        }
        [name] if topic_text(name).is_some() => {
            writeln!(out, "{}", topic_text(name).expect("checked above"))?;
        }
        path => {
            if let Some(mut subcommand) = help_command_for_path(cmd, path) {
                // `heddle help <command path>` falls through to that
                // command's clap-derived help so the contract on
                // `Commands::Help` holds for nested public paths too.
                drop(out);
                subcommand.print_help()?;
            } else {
                let name = path.join(" ");
                writeln!(
                    out,
                    "no topic or command '{name}'. Run `heddle help advanced` for \
                     the full advanced list, or `heddle help` for the \
                     curated everyday surface."
                )?;
            }
        }
    }
    Ok(())
}

pub fn print_direct_help_for_raw(
    cmd: &clap::Command,
    raw: &[String],
) -> Option<std::io::Result<()>> {
    let path = command_path_from_raw_help_request(cmd, raw)?;
    Some(match help_command_for_path(cmd, &path) {
        Some(mut subcommand) => subcommand.print_long_help(),
        None => print_help(cmd, &path),
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

/// Intercept `heddle capture --help-agent`: render `capture`'s
/// clap-derived help with the hidden agent-automation flags revealed.
/// Returns `None` when the request isn't a `capture --help-agent`
/// invocation, so `main` falls through to normal parsing.
pub fn print_capture_agent_help_for_raw(
    cmd: &clap::Command,
    raw: &[String],
) -> Option<std::io::Result<()>> {
    if !raw.iter().any(|arg| arg == "--help-agent") {
        return None;
    }
    let verb = raw.iter().find(|token| !token.starts_with('-'))?;
    let subcommand = find_subcommand_or_alias(cmd, verb)?;
    if subcommand.get_name() != "capture" {
        return None;
    }
    let bin_name = format!("{} {}", cmd.get_name(), subcommand.get_name());
    let mut help = reveal_capture_agent_flags(subcommand.clone()).bin_name(bin_name);
    Some(help.print_long_help())
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
        if let Some(arg) = current.get_arguments().find(|arg| {
            arg.get_long()
                .is_some_and(|long| token == &format!("--{long}"))
        }) {
            skip_next = arg.get_action().takes_values();
            continue;
        }
        if token.starts_with("--") {
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

/// Static per-topic help. Topics are addressed via `heddle help <topic>`.
pub fn topic_text(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "advanced" => ADVANCED_HELP,
        "agent-flags" => AGENT_FLAGS_TOPIC,
        "agent" | "daemon" => DAEMON_TOPIC,
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
status/diff/commit/start, ready/merge/ship/push/pull, undo, log/show,\n\
doctor/verify. Power nouns such as thread/workspace/remote/bridge/agent and\n\
Git adapter commands live behind this topic. Use `heddle help\n\
<verb>` for curated topics or `heddle <verb> --help` for the full clap-derived\n\
docs.\n\
\n\
This is intentional. The everyday surface stays minimal so first-time users aren't\n\
overwhelmed; agents and power users reach for the advanced affordances when they\n\
need them.\n";

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
    heddle merge <name> --preview
    heddle ship --thread <name>
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
`heddle ship` (or check readiness without merging via `heddle ready`).\n\
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
the workflow's point of view — `capture`, `ship`, `goto`, etc. behave\n\
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
- `heddle thread refresh <name>` — the same refresh, addressed by\n\
  thread name rather than the current checkout.\n\
- If `sync` reports conflicts or other blockers, use\n\
  `heddle thread resolve <name>` to walk through the next steps, or\n\
  `heddle conflict` to handle the conflicts as structured data.\n\
- `heddle ship` will refresh-then-merge for you when the replay is\n\
  clean; it fails closed when manual resolution is required.\n\
\n\
# `goto` vs. `git checkout` vs. `thread switch`\n\
\n\
These three look similar but operate at different layers:\n\
\n\
- `heddle thread switch <name>` — change which *thread* is active.\n\
  Each thread has its own checkout; switching may auto-capture\n\
  outstanding work on the thread you're leaving. Pair with the shell\n\
  hook (`heddle shell init`) to auto-cd into the target thread's\n\
  directory.\n\
- `heddle goto <state>` — move the *current thread's* worktree to a\n\
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
    heddle ready\n\
    cd -\n\
    heddle merge <name> --preview\n\
    heddle ship --thread <name> --no-push     # add --push when ready to publish\n\
\n\
Recover or prove state:\n\
\n\
    heddle undo\n\
    heddle verify\n\
\n\
State-specific recovery:\n\
\n\
    Worktree has unsaved edits: heddle commit -m \"...\"\n\
    Captured in Heddle but not Git: heddle checkpoint -m \"...\"\n\
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
    heddle merge <name> --preview\n\
    heddle ship --thread <name> --no-push     # add --push to land and push together\n\
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
            "inspect",
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
            "init", "clone", "status", "start", "commit", "ready", "diff", "merge", "ship",
            "resolve", "undo", "log", "show", "pull", "push", "doctor", "verify",
        ] {
            assert!(
                everyday.contains(verb),
                "`{verb}` is part of the core loop but is not advertised on \
                 the everyday surface"
            );
        }
        for verb in ["review", "discuss", "context", "goto", "thread", "bridge"] {
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

    /// heddle#278. `--help-agent` is intercepted only for `capture`.
    #[test]
    fn capture_help_agent_intercept_is_capture_scoped() {
        use clap::CommandFactory;
        let cmd = crate::cli::cli_args::Cli::command();
        let owned = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(
            print_capture_agent_help_for_raw(&cmd, &owned(&["capture", "--help-agent"])).is_some(),
            "capture --help-agent should be intercepted"
        );
        assert!(
            print_capture_agent_help_for_raw(&cmd, &owned(&["status", "--help-agent"])).is_none(),
            "--help-agent on a non-capture verb should fall through"
        );
        assert!(
            print_capture_agent_help_for_raw(&cmd, &owned(&["capture", "--help"])).is_none(),
            "plain --help should fall through to normal help"
        );
    }

    #[test]
    fn hidden_aliases_are_hidden() {
        for verb in ["gc", "index", "monitor"] {
            assert_eq!(tier_of(verb), Tier::Hidden, "{verb}");
        }
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
}
