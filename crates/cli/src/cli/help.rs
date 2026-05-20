// SPDX-License-Identifier: Apache-2.0
//! Progressive-disclosure help: curated default, advanced surface,
//! topic-scoped help.
//!
//! The Heddle CLI's default `heddle help` lists only everyday verbs.
//! Advanced affordances (checkpoint, query, conflict, hook, agent serve,
//! ephemeral threads) are reachable via
//! `heddle help advanced` or `heddle help <topic>`. Per-verb help via
//! `heddle <verb> --help` continues to derive from clap doc-comments.
//!
//! # Cultural deliverable
//!
//! The default help is **curated, not auto-generated**. Adding a verb
//! means picking a tier in [`tier_of`]; the exhaustive match is the
//! enforcement mechanism — forgetting a new verb is a build break.
//! See `AGENTS.md` "CLI surface curation" for the full doctrine.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Front-door verbs: `init, start, capture, merge, log, status,
    /// review, discuss, annotate, switch, undo, bridge`.
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
    match verb {
        // ── Everyday ──────────────────────────────────────────────
        "init" | "start" | "capture" | "merge" | "log" | "status" | "review" | "discuss"
        | "context" | "switch" | "undo" | "bridge" | "help" => Tier::Everyday,
        // The curated everyday set includes `annotate`. Heddle today
        // exposes annotation management under `context`
        // (set/get/list/edit). Map the missing front-door intent to
        // that subcommand surface for now.

        // ── Advanced ──────────────────────────────────────────────
        // `abort`, `continue`, `doctor`, `git-overlay`, `version` were
        // added by the codex git-overlay foundation; classified
        // Advanced because they appear in operator-loop scripts and
        // diagnostic flows rather than the everyday top-of-funnel.
        // `attempt`, `try`, `retro`, `schemas`, `redact`, `purge`
        // followed the same path — operator/security/diagnostic verbs
        // that script around the everyday loop without being part of
        // it. `redact` and `purge` in particular are security ops
        // (Biscuit-gated `redact:repo`/`purge:repo` capabilities);
        // they're explicitly NOT everyday verbs.
        "abort" | "agent" | "actor" | "attempt" | "auth" | "bisect" | "blame" | "checkpoint"
        | "cherry-pick" | "clean" | "clone" | "collapse" | "compare" | "completion"
        | "conflict" | "continue" | "daemon" | "delegate" | "diagnose" | "diff" | "doctor"
        | "fetch" | "fork" | "fsck" | "git-overlay" | "goto" | "hook" | "inspect"
        | "integration" | "maintenance" | "marker" | "presence" | "pull" | "purge" | "push"
        | "query" | "ready" | "rebase" | "redact" | "redo" | "remote" | "resolve" | "retro"
        | "revert" | "run" | "schemas" | "semantic" | "session" | "shell" | "ship" | "show"
        | "stash" | "store" | "support" | "sync" | "thread" | "try" | "version" | "watch"
        | "workspace" => Tier::Advanced,

        // ── Hidden ────────────────────────────────────────────────
        // `transaction` is hidden in alpha — buffered-op replay at
        // commit and rewind-on-abort are still follow-on work; the
        // verb stays available for testing but is not advertised.
        // `harness-bridge` is internal harness plumbing invoked via
        // env vars by `heddle run` and adapter shims — not a
        // user-facing verb.
        "gc" | "harness-bridge" | "index" | "monitor" | "transaction" => Tier::Hidden,

        // Anything unrecognised is treated as Advanced rather than
        // panicking. This preserves forward-compatibility for tools
        // that script around new verbs before the tier table catches up.
        _ => Tier::Advanced,
    }
}

/// Verbs that show in `heddle help`, in editorial order. Blurbs are
/// looked up at print time from each verb's clap `about` (its first
/// doc-comment line) — see [`about_first_line`]. Keeping only names
/// here means there's a single source of truth for command summaries.
pub fn everyday_verbs() -> &'static [&'static str] {
    &[
        "init", "start", "capture", "merge", "log", "status", "review", "discuss", "context",
        "undo", "bridge",
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
        "thread",
        "fork",
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
        "goto",
        "ready",
        "ship",
        "checkpoint",
        "sync",
        "delegate",
        "run",
        "continue",
        "abort",
        "diff",
        "marker",
        "workspace",
        "integration",
        "maintenance",
        "clone",
        "auth",
        "diagnose",
        "show",
        "query",
        "session",
        "actor",
        "store",
        "completion",
        "resolve",
        "presence",
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

/// Entry point for the `Commands::Help { topic }` dispatch arm
/// AND the bare-help intercept in `main.rs`. Routes between everyday
/// / advanced / topic surfaces and falls through to "use `--help`"
/// for verb names without a dedicated topic.
///
/// All output goes to stdout (this is help, not diagnostic). Returns
/// `Ok(())` even for unknown topics; the printer surfaces the
/// suggestion text rather than erroring.
pub fn print_help(cmd: &clap::Command, topic: Option<&str>) -> std::io::Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match topic {
        None => {
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
                "Run `heddle help advanced` for advanced commands or \
                 `heddle help <topic>` for a topic page (e.g. `daemon`, \
                 `signals`, `bridge`, `operation-ids`)."
            )?;
        }
        Some("advanced") => {
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
        Some(name) => {
            if let Some(body) = topic_text(name) {
                writeln!(out, "{}", body)?;
            } else if let Some(subcommand) = cmd.find_subcommand(name) {
                // `heddle help <verb>` falls through to that verb's
                // clap-derived help so the contract on `Commands::Help`
                // (`heddle help <verb>` → that verb's `--help`) holds.
                // We clone because `print_help` takes `&mut self` and
                // we only have a borrow of the parent. Set `bin_name`
                // explicitly so the rendered `Usage:` line says `heddle
                // <verb>` instead of just `<verb>` — the parent name
                // isn't otherwise carried through the clone.
                drop(out);
                let mut subcommand =
                    subcommand
                        .clone()
                        .bin_name(format!("{} {}", cmd.get_name(), name));
                subcommand.print_help()?;
            } else {
                writeln!(
                    out,
                    "no topic '{name}'. Run `heddle help advanced` for \
                     the full advanced list, or `heddle help` for the \
                     curated everyday surface."
                )?;
            }
        }
    }
    Ok(())
}

/// Static per-topic help. Topics are addressed via `heddle help <topic>`.
pub fn topic_text(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "advanced" => ADVANCED_HELP,
        "agent" | "daemon" => DAEMON_TOPIC,
        "operation-ids" | "idempotency" => OPERATION_IDS_TOPIC,
        "review" => REVIEW_TOPIC,
        "discuss" | "discussions" => DISCUSS_TOPIC,
        "bridge" | "footer" | "notes" => BRIDGE_TOPIC,
        "signals" | "risk-signals" => SIGNALS_TOPIC,
        _ => return None,
    })
}

const ADVANCED_HELP: &str = "Advanced verbs — see `heddle help advanced` for the complete list.\n\
\n\
The default `heddle help` curates the everyday surface (init, start, capture, merge,\n\
log, status, review, discuss, context, undo, bridge). Everything else lives behind\n\
this topic and `heddle help <verb> --help` for the full clap-derived docs.\n\
\n\
This is intentional. The everyday surface stays minimal so first-time users aren't\n\
overwhelmed; agents and power users reach for the advanced affordances when they\n\
need them.\n";

const DAEMON_TOPIC: &str =
    "Two daemons — both have legitimate uses; they are not interchangeable.\n\
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

const OPERATION_IDS_TOPIC: &str =
    "Idempotency — every state-changing call accepts a `client_operation_id`.\n\
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
            "operation-ids",
            "idempotency",
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
        let advanced: std::collections::HashSet<&str> =
            advanced_verbs().iter().copied().collect();
        for verb in ["query", "checkpoint", "continue", "abort"] {
            assert!(
                advanced.contains(verb),
                "`{verb}` is referenced in user-facing tips but is not \
                 advertised by `heddle help advanced`"
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
