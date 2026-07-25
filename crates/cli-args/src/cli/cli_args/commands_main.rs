// SPDX-License-Identifier: Apache-2.0
//! Top-level CLI commands.

use clap::{Args, Subcommand};

#[cfg(feature = "client")]
use super::AuthCommands;
#[cfg(feature = "semantic")]
use super::SemanticCommands;
use super::{
    AgentCommands, CompletionSubject, ContextCommands, DiscussCommands, HookCommands,
    IntegrationCommands, OplogCommands, QueryArgs, RedactCommands, RemoteCommands, ReviewCommands,
    ShellCommands, ThreadCommands, VisibilityCommands,
    commands_args::{
        AdoptArgs, CloneArgs, CollapseArgs, CommitArgs, DiffArgs, DoctorArgs, ExpandArgs, InitArgs,
        LandArgs, LogArgs, PullArgs, PushArgs, ReadyArgs, ResolveArgs, RetroArgs, RevertArgs,
        RunArgs, SnapshotArgs, SyncArgs, ThreadStartArgs, TimelineArgs, TryArgs, UndoArgs,
        WatchArgs,
    },
};
#[cfg(feature = "git-overlay")]
use super::{ExportCommands, ImportCommands};

#[derive(Clone, Debug, Args)]
pub struct FsckArgs {
    /// Full check (includes content verification).
    #[arg(long)]
    pub full: bool,

    /// Run slower graph and signature integrity checks.
    #[arg(long)]
    pub thorough: bool,

    /// Include Git projection, mapping, notes, and checkout checks.
    #[arg(long)]
    pub git: bool,

    #[command(subcommand)]
    pub command: Option<FsckCommands>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum FsckCommands {
    /// Repair an integrity surface, then verify it.
    Repair {
        #[command(subcommand)]
        target: FsckRepairCommands,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum FsckRepairCommands {
    /// Reconcile Git projection metadata or one projected ref.
    Git(FsckRepairGitArgs),
}

#[derive(Clone, Debug, Args)]
pub struct FsckRepairGitArgs {
    /// Git ref to reconcile. Required for native repositories.
    #[arg(long = "ref", value_name = "BRANCH")]
    pub ref_name: Option<String>,

    /// Assert the intended authority direction.
    #[arg(long, value_parser = ["git", "heddle"])]
    pub prefer: Option<String>,

    /// Show the authority-valid repair without changing refs.
    #[arg(long)]
    pub preview: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize Heddle in a directory or existing Git checkout.
    Init(InitArgs),

    /// Adopt Git history into Heddle-native source authority.
    ///
    /// Git Overlay is the normal existing-Git mode: Git keeps source objects,
    /// refs, index, and worktree state while Heddle stores metadata in
    /// `.heddle`. `adopt` imports history and moves source authority to Heddle.
    Adopt(AdoptArgs),

    /// Curated, progressive-disclosure help.
    ///
    /// `heddle help` prints the curated everyday verbs and points at
    /// `heddle help advanced` for everything else. `heddle help
    /// <topic>` prints the topic page (e.g. `daemon`, `signals`,
    /// `bridge`). `heddle help <command path>` falls through to that
    /// command's `--help` so the printer never duplicates clap's
    /// per-verb derivation.
    Help {
        /// Topic name (`advanced`, `daemon`, `signals`, …) or command
        /// path. When omitted, prints the curated default.
        #[arg(value_name = "TOPIC_OR_COMMAND")]
        topics: Vec<String>,
    },

    /// Show what needs attention and the next safe Heddle action.
    #[command(after_help = "\
Examples:
  heddle status               # current thread, dirty paths, recommended next step
  heddle status --short       # one-line summary for shell prompts
  heddle status --watch       # live dashboard that refreshes in place
")]
    Status {
        /// Short format.
        #[arg(short, long)]
        short: bool,

        /// Continuously refresh status.
        #[arg(long)]
        watch: bool,

        /// Internal helper for tests: stop after N watch updates.
        #[arg(long, hide = true)]
        watch_iterations: Option<usize>,

        /// Internal helper for tests: polling interval in milliseconds.
        #[arg(long, hide = true)]
        watch_interval_ms: Option<u64>,
    },

    /// Stream live oplog activity.
    ///
    /// Tails the repository's append-only oplog file like `tail -f`,
    /// emitting snapshots, merges, and thread events as they happen.
    /// Exits on Ctrl-C.
    Watch(WatchArgs),

    /// Verify this workspace; exits nonzero until every check is clean.
    #[command(after_help = "\
Checks: Git mapping, worktree, remote, operation, clone verification, machine contract.

Examples:
  heddle verify                # strict verification gate and next recovery step
  heddle verify --verbose      # full proof rows and machine-contract details
  heddle verify --output json  # proof JSON when clean; error envelope when blocked
")]
    Verify,

    /// Explain repository health, or run targeted doctor checks.
    ///
    /// `heddle doctor` (no subcommand) reports repository health and
    /// the next recovery step. `heddle doctor docs` diff-checks markdown
    /// documentation against
    /// the actual CLI surface and exits non-zero on drift — wire it
    /// into CI to stop docs from going stale.
    Doctor(DoctorArgs),

    /// Print the JSON Schema for a `--output json`-emitting verb.
    ///
    /// Contract-table introspection over CLI output shapes —
    /// useful when wiring tools that consume `heddle <verb>
    /// --output json` and want to validate or generate types. The schemas
    /// live in `crates/cli/src/cli/commands/schemas.rs`; the
    /// command contract table registers available and documented
    /// schema verbs for `heddle doctor schemas` drift detection
    /// against `docs/json-schemas.md`.
    ///
    /// With no `<verb>`, prints the registered schema verbs. `<verb>`
    /// is the joined subcommand path — e.g. `status`, `log`,
    /// `fsck repair git`, `marker list`.
    #[command(visible_alias = "schema")]
    Schemas {
        /// The verb whose schema to emit. Run `heddle schemas --help`
        /// or look at `docs/json-schemas.md` for the registered list.
        ///
        /// `trailing_var_arg = true` lets the verb spec carry literal
        /// `--flag` tokens (e.g. `heddle schemas log --reflog`,
        /// `heddle schemas marker delete --prefix`) without clap
        /// parsing them as options on `schemas` itself.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        verb: Vec<String>,
    },

    /// Create or resume an isolated thread for focused work.
    Start(ThreadStartArgs),

    /// Run a command in a sandboxed ephemeral thread.
    ///
    /// Heddle creates a fresh thread with an isolated checkout, runs
    /// `<cmd>` inside it, and then either captures the result on a
    /// zero exit or drops the thread on a non-zero exit. The parent
    /// thread's working tree is never touched — the ephemeral thread
    /// is the sandbox. Implements item 3.1 from the heddle 6→8 plan.
    ///
    /// `try` is the **new-sandbox** sibling to `run`. Reach for `run`
    /// when you already have a thread and just want to exec a command
    /// inside its checkout (no thread creation, no capture, no
    /// rollback).
    Try(TryArgs),

    /// Automation/workflow command: run a command inside an existing
    /// thread's execution root.
    ///
    /// `run` is the **existing-thread** sibling to `try`. It looks up
    /// the named (or current) thread, sets the child's cwd to that
    /// thread's checkout, exports `HEDDLE_THREAD_*`, and runs `<cmd>`.
    /// It does NOT create a thread, capture
    /// state on success, or roll back on failure — those are `try`'s
    /// job. Reach for `try` when you want the sandbox lifecycle; reach
    /// for `run` when you already have a thread and just need to exec
    /// inside it.
    Run(RunArgs),

    /// Automation/workflow command: refresh the current thread onto its target when safe.
    Sync(SyncArgs),

    /// Continue the active operation without remembering the specific subcommand.
    Continue,

    /// Abort the active operation without remembering the specific subcommand.
    Abort,

    /// Integrate a ready thread into its local target.
    ///
    /// `land` is the local integration verb: capture outstanding work if needed,
    /// refresh against the target when safe, and land the thread. It fails
    /// closed when conflicts or other blockers exist. Pair it with `ready`
    /// when you want the verdict and next action before landing anything.
    Land(LandArgs),

    /// Prepare this thread for review or merge.
    ///
    /// `ready` captures outstanding work if needed, checks conflicts,
    /// blockers, freshness, and semantic risk, then marks the thread
    /// ready or blocked and prints the next action. It never lands,
    /// checkpoints, or pushes; use it when you want Heddle's verdict
    /// before integrating the work.
    Ready(ReadyArgs),

    /// Capture a recoverable Heddle step for undo, provenance, and review.
    Capture(SnapshotArgs),

    /// Commit the current captured state to the authoritative Git checkout.
    Commit(CommitArgs),

    /// Show state history.
    ///
    /// By default, when a thread name is given (e.g. `heddle log master`),
    /// the walk is *first-parent only* — equivalent to `git log
    /// --first-parent <branch>`. To see every ancestor reachable through
    /// merge commits, pass `--graph` (which renders the full DAG) or
    /// `--all` (which lists every state regardless of ancestry).
    #[command(visible_alias = "history")]
    Log(LogArgs),

    /// Navigate, fork, reset, and recover agent tool-call timelines.
    #[command(after_help = "\
Examples:
  heddle log --timeline
  heddle timeline fork --tool-call call_123 --branch tlb-alt
  heddle timeline reset --step tls-abc --materialize
  heddle timeline recover
")]
    Timeline(TimelineArgs),

    /// Show state details.
    Show {
        /// State by physical state ID, logical change ID, or unambiguous prefix.
        /// Defaults to HEAD.
        state: Option<String>,
    },

    /// Summarize a working session.
    ///
    /// Combines oplog, agent registry, marker, and context-annotation
    /// reads into one structured payload — agent-readable retro of
    /// captures, signals, and notable events since `--since`. Replaces
    /// the reconstruct-from-`heddle log` boilerplate.
    Retro(RetroArgs),

    /// Show what changed in the worktree, a thread, or two states.
    Diff(DiffArgs),

    /// Open or resolve discussions anchored to symbols.
    ///
    /// Open a discussion against a symbol; append turns;
    /// resolve by edit or dismiss. Anchors
    /// travel across renames and cross-file moves on subsequent
    /// state mutations.
    #[command(after_help = "\
Examples:
  heddle discuss open src/auth.rs verify 'Should this reject expired tokens?'  # anchor a discussion
  heddle discuss append <id> 'switched to argon2'          # add a turn
  heddle discuss resolve <id> --mode by-edit --state HEAD
")]
    Discuss {
        #[command(subcommand)]
        command: DiscussCommands,
    },

    /// Structured query over the operation log. Filter by
    /// actor, time window, signal kind, symbol, thread, verbs. Returns
    /// structured results consumable by agents.
    Query(QueryArgs),

    /// Review a state — render the payload, sign, see signal health.
    ///
    /// `heddle review show` renders the review payload (summary,
    /// agent narrative, in-budget signals, anchored discussions).
    /// `heddle review sign` submits a `read` / `agent_preview` /
    /// `agent_co_review` signature on the state. `heddle review
    /// health` reports per-module signal fire rates over a rolling
    /// window.
    #[command(after_help = "\
Examples:
  heddle review show HEAD                                # render the review payload for HEAD
  heddle review sign HEAD --kind read --public-key <hex> --signature <hex> --signed-at-unix <ts>
  heddle review health --window 7                       # signal fire-rates over recent states
")]
    Review {
        #[command(subcommand)]
        command: ReviewCommands,
    },

    /// Redact a sensitive blob in a state so reads return a stub
    /// instead of the content.
    ///
    /// `heddle redact apply` declares a redaction; the blob bytes stay
    /// on disk and reads return the operator-supplied stub. `heddle
    /// purge` afterward physically removes the bytes. Both are signed,
    /// attributed, oplog-audited operations. See
    /// `docs/PRINCIPLES.md` (the honesty principle) for context.
    Redact {
        #[command(subcommand)]
        command: RedactCommands,
    },

    /// Declare and inspect a state's audience visibility tier.
    ///
    /// `heddle visibility set` binds a tier to a state; `promote` lifts it to
    /// a less-restrictive tier via a superseding record; `show` reports the
    /// effective tier; `list` enumerates non-public states. Capture binds the
    /// inherited `[review.discussion] default_visibility` automatically
    /// (Invariant A) — these verbs are the explicit operator overrides.
    Visibility {
        #[command(subcommand)]
        command: VisibilityCommands,
    },

    /// Revert changes from a state.
    Revert(RevertArgs),

    /// Undo the last Heddle operation.
    Undo(UndoArgs),

    /// Collapse (squash) multiple states into one.
    Collapse(CollapseArgs),

    /// Expand a squashed land into the captures it collapsed.
    Expand(ExpandArgs),

    /// Manage threads.
    Thread {
        #[command(subcommand)]
        command: ThreadCommands,
    },

    /// Shell integration helpers (auto-cd on thread start/switch/cd).
    Shell {
        #[command(subcommand)]
        command: ShellCommands,
    },

    /// Internal shell-completion candidate helper.
    #[command(name = "complete", alias = "__complete", hide = true)]
    Complete {
        /// Candidate set to print, one candidate per line.
        #[arg(value_enum)]
        subject: CompletionSubject,
    },

    /// Resolve merge conflicts.
    Resolve(ResolveArgs),

    /// Verify repository integrity or explicitly repair one surface.
    Fsck(FsckArgs),

    /// Inspect and repair the operation log.
    ///
    /// `heddle oplog recover` explicitly salvages a truncated or torn oplog,
    /// reporting what was recovered — the operator-facing entrypoint over the
    /// same recovery the everyday read path runs automatically.
    Oplog {
        #[command(subcommand)]
        command: OplogCommands,
    },

    /// Import from another version control system.
    #[cfg(feature = "git-overlay")]
    Import {
        #[command(subcommand)]
        command: ImportCommands,
    },

    /// Export to another version control system.
    #[cfg(feature = "git-overlay")]
    Export {
        #[command(subcommand)]
        command: ExportCommands,
    },

    /// Push the source-authoritative history to a remote.
    Push(PushArgs),

    /// Pull source-authoritative history from a remote.
    Pull(PullArgs),

    /// Manage remote repositories.
    Remote {
        #[command(subcommand)]
        command: RemoteCommands,
    },

    /// Authenticate with a Heddle server.
    #[cfg(feature = "client")]
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },

    /// Report the acting identity (principal, token kind, scopes, operation
    /// ceiling, TTL, signing status, server reachability).
    #[cfg(feature = "client")]
    #[command(after_help = "\
Examples:
  heddle whoami                       # human-readable identity summary
  heddle whoami --output json         # machine-readable, stable output_kind shape
  heddle whoami --server grpc.heddle.sh")]
    Whoami {
        /// Heddle server address (defaults to the configured server).
        #[arg(long)]
        server: Option<String>,
    },

    /// Manage code context annotations.
    #[command(after_help = "\
Examples:
  heddle context set --path src/auth.rs --scope symbol:verify --kind invariant -m 'returns false on timing mismatch'
  heddle context get --path src/auth.rs --scope symbol:verify
  heddle context list --prefix src/auth          # everything attached under a path
  heddle context check --path src/auth.rs        # surface annotations for editor tooling
")]
    Context {
        #[command(subcommand)]
        command: ContextCommands,
    },

    /// Manage ambient harness integrations.
    Integration {
        #[command(subcommand)]
        command: IntegrationCommands,
    },

    /// Semantic analysis queries (call-graph hot-spots, churn,
    /// signature-stability surfaces).
    #[cfg(feature = "semantic")]
    Semantic {
        #[command(subcommand)]
        command: SemanticCommands,
    },

    /// FUSE mount-daemon control plane — distinct from `agent`.
    ///
    /// `heddle daemon serve` runs a foreground mount daemon that
    /// owns FUSE sessions for `--workspace virtualized --daemon`
    /// threads. It is normally spawned on demand by the per-thread
    /// CLI; running it interactively is for debugging.
    /// `status` reports liveness/uptime/mount count without spawning;
    /// `stop` asks a running daemon to drain mounts and exit.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },

    /// Agent control surface — daemon lifecycle and reservation API.
    ///
    /// `heddle agent serve|status|stop` controls the local gRPC
    /// daemon (Unix socket inside the repo). `heddle agent
    /// reserve|capture|ready|release|list|heartbeat` is the stable
    /// JSON contract orchestrators use to coordinate parallel
    /// writers. Distinct from `heddle daemon` (FUSE mount control
    /// plane) — different subsystem.
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },

    /// Inspect and refresh rebuildable performance sidecars.
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommands,
    },

    /// Clone from remote.
    Clone(CloneArgs),

    /// Manage repository hooks.
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
}

/// Maintenance subcommands.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum MaintenanceCommands {
    /// Inspect repository performance sidecars and repo shape.
    Inspect,

    /// Refresh repository performance sidecars without changing repository meaning.
    Refresh,

    /// Garbage collect unreachable objects.
    Gc {
        /// Prune unreachable objects.
        #[arg(long)]
        prune: bool,

        /// Aggressive garbage collection.
        #[arg(long)]
        aggressive: bool,

        /// Show what would be removed without removing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Daemon control plane subcommands. See `Commands::Daemon`.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum DaemonCommands {
    /// Run a foreground mount daemon for this repository.
    ///
    /// Normally spawned on demand by the per-thread CLI when
    /// `--daemon` is passed. Running interactively is for
    /// debugging the daemon protocol.
    Serve,

    /// Report daemon liveness, version, uptime, and active mount
    /// count. No-op success when the daemon isn't running.
    Status,

    /// Ask the running daemon to drain its mounts and exit. Sweeps
    /// any leftover registry entries with `fusermount -u` as a
    /// safety net before returning.
    Stop,
}
