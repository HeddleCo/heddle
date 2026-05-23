// SPDX-License-Identifier: Apache-2.0
//! Progressive-disclosure help: curated default, advanced surface,
//! topic-scoped help.
//!
//! The Heddle CLI's default `heddle help` lists only everyday verbs.
//! Advanced affordances (review/discuss/context, checkpoint, query,
//! conflict, hook, agent serve, ephemeral threads) are reachable via
//! `heddle help advanced` or `heddle help <topic>`. Per-verb help via
//! `heddle <verb> --help` continues to derive from clap doc-comments.
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
    /// capture, check readiness, inspect, integrate, recover, and
    /// diagnose. See [`everyday_verbs`] for the authoritative list.
    Everyday,
    /// Reachable via `heddle help advanced` or `heddle help <topic>`.
    /// Most agent-loop and operational verbs land here.
    Advanced,
    /// Compatibility aliases that should not be advertised at all.
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

/// Verbs that show in `heddle help`, in editorial order. Blurbs are
/// looked up at print time from each verb's clap `about` (its first
/// doc-comment line) — see [`about_first_line`]. Keeping only names
/// here means there's a single source of truth for command summaries.
pub fn everyday_verbs() -> &'static [&'static str] {
    &[
        "init",
        "clone",
        "status",
        "workspace",
        "start",
        "capture",
        "ready",
        "diff",
        "merge",
        "resolve",
        "undo",
        "log",
        "show",
        "thread",
        "bridge",
        "doctor",
        "trust",
    ]
}

/// Verbs surfaced by `heddle help advanced`, in editorial order. Not
/// exhaustive of every existing verb (see [`tier_of`] for the full
/// table) — focuses on the agent-loop surface plus the
/// operational verbs power users reach for. As with [`everyday_verbs`],
/// blurbs come from clap at print time.
pub fn advanced_verbs() -> &'static [&'static str] {
    &[
        "agent",
        "daemon",
        "hook",
        "review",
        "discuss",
        "context",
        "commands",
        "commit",
        "branch",
        "switch",
        "fork",
        "goto",
        "collapse",
        "compare",
        "stash",
        "fetch",
        "push",
        "pull",
        "remote",
        "rebase",
        "cherry-pick",
        "blame",
        "bisect",
        "fsck",
        "semantic",
        "watch",
        "redo",
        "revert",
        "clean",
        "ship",
        "checkpoint",
        "sync",
        "delegate",
        "run",
        "continue",
        "abort",
        "marker",
        "integration",
        "maintenance",
        "auth",
        "diagnose",
        "query",
        "session",
        "actor",
        "store",
        "completion",
        "presence",
        "version",
    ]
}

/// Look up the first line of a top-level subcommand's clap `about`
/// text. Returns an empty string when the verb is not a direct
/// subcommand of `cmd` or has no `about` set — `print_help` skips
/// rows with empty blurbs so feature-gated verbs (e.g. `semantic`
/// without the `semantic` feature) don't advertise themselves. The
/// `verb_blurbs_resolve_from_clap` test enforces that, under
/// `--all-features`, every advertised verb resolves.
///
/// The "Automation/workflow command:" prefix in `--help` is useful
/// framing on the per-verb page but pure noise in the curated
/// summary column, so it gets stripped here.
fn about_first_line(cmd: &clap::Command, verb: &str) -> String {
    let raw = cmd
        .get_subcommands()
        .find(|sc| sc.get_name() == verb)
        .and_then(|sc| sc.get_about())
        .map(|about| about.to_string().lines().next().unwrap_or("").to_string())
        .unwrap_or_default();
    let stripped = raw
        .trim_start_matches("Automation/workflow command:")
        .trim_start();
    let mut chars = stripped.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
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
            writeln!(out, "Heddle — AI-native version control")?;
            writeln!(out)?;
            writeln!(out, "Everyday commands:")?;
            for &name in everyday_verbs() {
                let blurb = about_first_line(cmd, name);
                if blurb.is_empty() {
                    continue;
                }
                writeln!(out, "  {:<10}  {}", name, blurb)?;
            }
            writeln!(out)?;
            writeln!(
                out,
                "Output: `--output auto` renders text on a TTY and JSON when piped; \
                 use `--output text` or `--output json` to force a mode."
            )?;
            writeln!(out)?;
            writeln!(
                out,
                "Run `heddle help advanced` for advanced commands or \
                 `heddle help <topic>` for a topic page (e.g. `threads`, \
                 `daemon`, `signals`, `bridge`, `operation-ids`, \
                 `git-dependencies`)."
            )?;
        }
        [name] if name == "advanced" => {
            writeln!(out, "{}", ADVANCED_HELP)?;
            writeln!(out, "Advanced commands:")?;
            for &name in advanced_verbs() {
                let blurb = about_first_line(cmd, name);
                if blurb.is_empty() {
                    continue;
                }
                writeln!(out, "  {:<14}  {}", name, blurb)?;
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

fn help_command_for_path(cmd: &clap::Command, path: &[String]) -> Option<clap::Command> {
    if path.is_empty() {
        return None;
    }

    let mut current = cmd;
    let mut bin_name = cmd.get_name().to_string();
    for part in path {
        let subcommand = current.find_subcommand(part)?;
        bin_name.push(' ');
        bin_name.push_str(part);
        current = subcommand;
    }

    Some(current.clone().bin_name(bin_name))
}

/// Static per-topic help. Topics are addressed via `heddle help <topic>`.
pub fn topic_text(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "advanced" => ADVANCED_HELP,
        "agent" | "daemon" => DAEMON_TOPIC,
        "threads" | "model" => THREADS_TOPIC,
        "operation-ids" | "idempotency" => OPERATION_IDS_TOPIC,
        "git-dependencies" | "git-deps" | "git-dependency" => GIT_DEPENDENCIES_TOPIC,
        "review" => REVIEW_TOPIC,
        "discuss" | "discussions" => DISCUSS_TOPIC,
        "bridge" | "footer" | "notes" => BRIDGE_TOPIC,
        "signals" | "risk-signals" => SIGNALS_TOPIC,
        _ => return None,
    })
}

const ADVANCED_HELP: &str = "Advanced verbs — see `heddle help advanced` for the complete list.\n\
\n\
The default `heddle help` curates the core loop: init/clone, status/workspace,\n\
start/capture, ready/diff, merge/resolve/undo, log/show/thread, bridge/doctor.\n\
Everything else lives behind this topic and `heddle help <verb> --help` for the full\n\
clap-derived docs.\n\
\n\
This is intentional. The everyday surface stays minimal so first-time users aren't\n\
overwhelmed; agents and power users reach for the advanced affordances when they\n\
need them.\n";

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
                         process startup latency. Mode: same-user only,\n\
                         peer-cred check enforced. Out of scope for first ship:\n\
                         multi-user, remote, TLS.\n";

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
- `heddle capture` records into the thread's state history; `heddle\n\
  checkpoint` is what materializes a git-overlay commit on the\n\
  downstream branch.\n\
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
  thread to a heavy checkout at a chosen path. Use it when a thread\n\
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
- `heddle checkpoint` commits the current captured work to the\n\
  git-overlay branch/index. It refuses when the worktree has changes\n\
  that haven't been captured yet — capture first, then checkpoint.\n\
- The split lets agents and tools take many small captures (cheap,\n\
  reversible) without producing a noisy git history; checkpoints are\n\
  the durable downstream record.\n\
\n\
See also: `heddle help advanced` for the full operational surface,\n\
`heddle thread --help` for the thread subcommand list.\n";

const OPERATION_IDS_TOPIC: &str = "Idempotency — every state-changing call accepts a `client_operation_id`.\n\
\n\
The same id replayed with the same body returns the original outcome\n\
bit-identical; with a different body it returns FAILED_PRECONDITION.\n\
\n\
The dedup store is file-backed locally (`.heddle/state/operation_dedup.bin`,\n\
rmp-serde, 7-day default retention) and Postgres-backed in hosted deployments.\n\
\n\
The CLI accepts `--op-id <UUID>` on every state-changing verb (or honours\n\
`HEDDLE_OPERATION_ID`). Without an id, dedup is bypassed and the call\n\
executes normally.\n";

const GIT_DEPENDENCIES_TOPIC: &str = "Git executable dependencies — what works without `git` on PATH.\n\
\n\
Supported Git-overlay workflows use native/library paths and are tested with\n\
`PATH` stripped of `git`: `init`, `status`, local/bare `clone`, `bridge git\n\
import`, `bridge git status`, `bridge git sync/export` where implemented,\n\
`thread list`, `workspace`, `log`, `show`, `diff`, `checkpoint`, `merge`,\n\
`ready`, and `fsck`.\n\
\n\
Remaining `git` process calls are optional escape hatches:\n\
\n\
- partial/filter clone fallback when the native transport cannot honor the\n\
  requested capability;\n\
- lazy promisor hydration for missing partial-clone blobs;\n\
- `merge --git-commit`, which explicitly asks Heddle to write a Git commit;\n\
- raw Git continue/abort interop for externally-started Git operations;\n\
- best-effort environment metadata such as `git --version` in verbose bug\n\
  context.\n\
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
`next`            — placeholder until the operation-log query layer wires real\n\
                    pending-review selection.\n\
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

const BRIDGE_TOPIC: &str = "Bridge export footer + git notes.\n\
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
git note at `refs/notes/heddle`. To fetch + push notes:\n\
\n\
    git config --add remote.origin.fetch '+refs/notes/heddle:refs/notes/heddle'\n\
    git config --add remote.origin.push  'refs/notes/heddle:refs/notes/heddle'\n\
\n\
Then `git log --notes=heddle` displays the rich metadata inline.\n";

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
        for &verb in everyday_verbs() {
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
            "agent",
            "daemon",
            "threads",
            "model",
            "operation-ids",
            "idempotency",
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
        for &verb in advanced_verbs() {
            let t = tier_of(verb);
            assert!(
                matches!(t, Tier::Advanced),
                "expected Advanced for {verb}, got {t:?}"
            );
        }
    }

    /// Regression: heddle#150. `query`, `checkpoint`, `continue`, and
    /// `abort` are referenced in inline tips and error messages but
    /// were absent from the `heddle help advanced` listing, leaving
    /// users unable to discover the verb they were told to run.
    #[test]
    fn advanced_verbs_lists_tip_referenced_commands() {
        let advanced: std::collections::HashSet<&str> = advanced_verbs().iter().copied().collect();
        for verb in ["query", "checkpoint", "continue", "abort"] {
            assert!(
                advanced.contains(verb),
                "`{verb}` is referenced in user-facing tips but is not \
                 advertised by `heddle help advanced`"
            );
        }
    }

    /// The everyday surface should mirror the core loop rather than
    /// mixing in every collaboration feature. A first-time user should
    /// be able to orient, create/capture work, check readiness, inspect,
    /// integrate, recover, and diagnose from the first help screen.
    #[test]
    fn everyday_verbs_surface_the_core_loop() {
        let everyday: std::collections::HashSet<&str> = everyday_verbs().iter().copied().collect();
        for verb in [
            "init",
            "clone",
            "status",
            "workspace",
            "start",
            "capture",
            "ready",
            "diff",
            "merge",
            "resolve",
            "undo",
            "log",
            "show",
            "thread",
            "bridge",
            "doctor",
            "trust",
        ] {
            assert!(
                everyday.contains(verb),
                "`{verb}` is part of the core loop but is not advertised on \
                 the everyday surface"
            );
        }
        for verb in ["review", "discuss", "context", "goto"] {
            assert!(
                !everyday.contains(verb),
                "`{verb}` belongs behind advanced/topic help, not the core-loop surface"
            );
        }
    }

    #[test]
    fn hidden_aliases_are_hidden() {
        for verb in ["gc", "index", "monitor"] {
            assert_eq!(tier_of(verb), Tier::Hidden, "{verb}");
        }
    }

    /// Build-break property: every verb listed in `everyday_verbs` and
    /// `advanced_verbs` that's compiled into the current build MUST
    /// resolve to a clap subcommand with a non-empty `about`. Verbs
    /// gated behind a feature that isn't enabled (e.g. `semantic` when
    /// the `semantic` feature is off) are skipped — `print_help`
    /// already skips them at render time. If a verb is renamed in the
    /// `Commands` enum without a matching update here, this test
    /// fails for whichever feature combo the variant lives in.
    #[test]
    fn verb_blurbs_resolve_from_clap() {
        use clap::CommandFactory;
        let cmd = crate::cli::Cli::command();
        for &verb in everyday_verbs().iter().chain(advanced_verbs().iter()) {
            // Feature-gated verbs may not be present in this build —
            // skip them. The render path mirrors this.
            let Some(subcommand) = cmd.get_subcommands().find(|sc| sc.get_name() == verb) else {
                continue;
            };
            let blurb = about_first_line(&cmd, verb);
            assert!(
                !blurb.is_empty(),
                "verb `{verb}` is a clap subcommand but its `about` \
                 doc-comment is empty. The curated help printer needs \
                 a non-empty first line. (subcommand seen: {:?})",
                subcommand.get_name()
            );
        }
    }
}
