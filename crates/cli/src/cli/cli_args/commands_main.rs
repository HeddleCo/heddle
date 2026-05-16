// SPDX-License-Identifier: Apache-2.0
//! Top-level CLI commands.

use clap::Subcommand;

#[cfg(feature = "git-overlay")]
use super::BridgeCommands;
#[cfg(feature = "semantic")]
use super::SemanticCommands;
use super::{
    commands_args::{
        ActorDoneArgs, ActorExplainArgs, ActorListArgs, ActorShowArgs, ActorSpawnArgs, AttemptArgs,
        CloneArgs, CollapseArgs, DelegateArgs, DiagnoseArgs, DiffArgs, DoctorArgs, InitArgs,
        LogArgs, MergeArgs, PullArgs, PushArgs, ReadyArgs, ResolveArgs, RetroArgs, RevertArgs,
        RunArgs, SessionEndArgs, SessionListArgs, SessionSegmentArgs, SessionShowArgs,
        SessionStartArgs, ShipArgs, SnapshotArgs, SyncArgs, ThreadStartArgs, TryArgs, UndoArgs,
        WatchArgs,
    },
    AgentCommands, BisectCommands, CheckpointArgs, ConflictCommands, ContextCommands,
    DiscussCommands, HookCommands, IntegrationCommands, MarkerCommands, PurgeCommands, QueryArgs,
    RedactCommands, RemoteCommands, ReviewCommands, ShellCommands, StashCommands, ThreadCommands,
    TransactionCommands, WorkspaceCommands,
};
#[cfg(feature = "client")]
use super::{AuthCommands, SupportCommands};

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new Heddle repository.
    Init(InitArgs),

    /// Curated, progressive-disclosure help.
    ///
    /// `heddle help` prints the twelve everyday verbs and points at
    /// `heddle help advanced` for everything else. `heddle help
    /// <topic>` prints the topic page (e.g. `daemon`, `signals`,
    /// `bridge`). `heddle help <verb>` falls through to that verb's
    /// `--help` so the printer never duplicates clap's per-verb
    /// derivation.
    Help {
        /// Topic name (`advanced`, `daemon`, `signals`, …) or verb
        /// name. When omitted, prints the curated default.
        topic: Option<String>,
    },

    /// Show repository status.
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

    /// Diagnose repository, thread, actor, and worktree context.
    Diagnose(DiagnoseArgs),

    /// Explain repository health, or run targeted doctor checks.
    ///
    /// `heddle doctor` (no subcommand) reports repository health and
    /// the next recovery step (the same payload as `heddle diagnose`).
    /// `heddle doctor docs` diff-checks markdown documentation against
    /// the actual CLI surface and exits non-zero on drift — wire it
    /// into CI to stop docs from going stale.
    Doctor(DoctorArgs),

    /// Show the low-friction Git-overlay workflow.
    #[cfg(feature = "git-overlay")]
    GitOverlay,

    /// Print the JSON Schema for a `--json`-emitting verb.
    ///
    /// Single-registry introspection over CLI output shapes —
    /// useful when wiring tools that consume `heddle <verb>
    /// --json` and want to validate or generate types. The schemas
    /// live in `crates/cli/src/cli/commands/schemas.rs`; the same
    /// registry powers `heddle doctor schemas` for drift detection
    /// against `docs/json-schemas.md`.
    ///
    /// `<verb>` is the joined subcommand path — e.g. `status`, `log`,
    /// `bridge git status`, `marker list`.
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

    /// Show Heddle version. Pass global `--verbose` for bug-report context.
    Version,

    /// Start a thread for work in Heddle's primary thread-first workflow.
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

    /// Run a command in N parallel sandboxed ephemeral threads and
    /// rank the results.
    ///
    /// Best-of-N parallelism: each attempt gets its own ephemeral
    /// thread + isolated checkout, runs `<cmd>` inside it, and the
    /// results are ranked (primary exit code → optional `--evaluate`
    /// exit → diff size → duration). Failed attempts are dropped
    /// automatically; successful ones stay around for the user to
    /// merge or drop. Implements item 3.2 from the heddle 6→8 plan.
    Attempt(AttemptArgs),

    /// Automation/workflow command: run a command inside an existing
    /// thread's execution root.
    ///
    /// `run` is the **existing-thread** sibling to `try`. It looks up
    /// the named (or current) thread, sets the child's cwd to that
    /// thread's checkout, exports `HEDDLE_THREAD_*` and harness-bridge
    /// env, and runs `<cmd>`. It does NOT create a thread, capture
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

    /// Automation/workflow command: capture, integrate, and optionally
    /// push the current thread.
    ///
    /// `ship` is the **do-it** verb: capture outstanding work, refresh
    /// against the target if stale, merge into the parent (or
    /// `--remote` push target), checkpoint, and optionally push. It
    /// fails closed when conflicts or other blockers exist. Pair with
    /// `ready` (the **check-only** counterpart) when you want to know
    /// whether a ship would succeed without performing the integration.
    Ship(ShipArgs),

    /// Automation/workflow command: fan a parent thread into one or
    /// more delegated child threads.
    ///
    /// `delegate <task>...` is the multi-task fan-out wrapper around
    /// `start`. Each `<task>` is either `task` or `task:provider:model`,
    /// so you can race different agents on the same prompt in one
    /// command. It pre-warms the canonical store before materializing N
    /// child checkouts (relevant for the multi-task case), and stamps
    /// the spawned threads with `--parent-thread <current>`. For a
    /// single child with no per-task agent override, `heddle start
    /// <name>` is the lower-level path.
    Delegate(DelegateArgs),

    /// Automation/workflow command: capture work, evaluate merge
    /// readiness, and update thread state.
    ///
    /// `ready` is the **check-only** counterpart to `ship`. It captures
    /// any outstanding work, runs the same readiness preflight `ship`
    /// uses (conflicts, blockers, freshness, semantic risk), and writes
    /// `Ready` or `Blocked` onto the thread's state — but it never
    /// merges, never checkpoints, and never pushes. Reach for it when
    /// you want a verdict without committing to the integration.
    Ready(ReadyArgs),

    /// Capture a recoverable Heddle step for undo, provenance, and review.
    Capture(SnapshotArgs),

    /// Commit the current captured work to the Git-overlay branch/index.
    Checkpoint(CheckpointArgs),

    /// Show state history.
    ///
    /// By default, when a thread name is given (e.g. `heddle log master`),
    /// the walk is *first-parent only* — equivalent to `git log
    /// --first-parent <branch>`. To see every ancestor reachable through
    /// merge commits, pass `--graph` (which renders the full DAG) or
    /// `--all` (which lists every state regardless of ancestry).
    Log(LogArgs),

    /// Show state details.
    Show {
        /// State by change ID or hash prefix.
        state: String,
    },

    /// Summarize a working session.
    ///
    /// Combines oplog, agent registry, marker, and context-annotation
    /// reads into one structured payload — agent-readable retro of
    /// captures, signals, and notable events since `--since`. Replaces
    /// the reconstruct-from-`heddle log` boilerplate.
    Retro(RetroArgs),

    /// Inspect a state or thread (default: current thread).
    Inspect {
        /// State ID or thread name.
        target: Option<String>,
    },

    /// Move worktree to a state.
    Goto {
        /// Target state.
        target: String,

        /// Discard uncommitted changes.
        #[arg(short, long)]
        force: bool,
    },

    /// Remove untracked files from worktree.
    Clean {
        /// Actually remove files (required for safety).
        #[arg(short, long)]
        force: bool,

        /// Only show what would be removed.
        #[arg(long)]
        dry_run: bool,
    },

    /// Show differences between states.
    Diff(DiffArgs),

    /// Open or resolve discussions anchored to symbols.
    ///
    /// Open a discussion against a symbol; append turns;
    /// resolve into an annotation, by edit, or dismissed. Anchors
    /// travel across renames and cross-file moves on subsequent
    /// state mutations.
    #[command(after_help = "\
Examples:
  heddle discuss open --path src/auth.rs --symbol verify   # anchor a discussion to a symbol
  heddle discuss append <id> 'switched to argon2'          # add a turn
  heddle discuss resolve <id> --as annotation              # close into a context annotation
")]
    Discuss {
        #[command(subcommand)]
        command: DiscussCommands,
    },

    /// Structured query over the operation log. Filter by
    /// actor, time window, signal kind, symbol, thread, verbs. Returns
    /// structured results consumable by agents.
    Query(QueryArgs),

    /// Transactional multi-step edits. Begin, commit, abort,
    /// status. Operations within don't produce intermediate states.
    ///
    /// Hidden in alpha: buffered-op replay at commit and rewind-on-abort
    /// are still follow-on work; the verb stays available for testing
    /// but is not advertised in `heddle help advanced`.
    #[command(hide = true)]
    Transaction {
        #[command(subcommand)]
        command: TransactionCommands,
    },

    /// Structured conflicts. List, show, resolve conflicts as
    /// data — agents resolve programmatically without parsing markers.
    Conflict {
        #[command(subcommand)]
        command: ConflictCommands,
    },

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
  heddle review show HEAD                  # render the review payload for HEAD
  heddle review sign HEAD --as read        # acknowledge a review
  heddle review health --window 7d         # signal fire-rates over the last week
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

    /// Physically remove the bytes referenced by an existing redaction.
    /// Irreversible; refuses without `--force`.
    Purge {
        #[command(subcommand)]
        command: PurgeCommands,
    },

    /// Revert changes from a state.
    Revert(RevertArgs),

    /// Undo last operation.
    Undo(UndoArgs),

    /// Redo undone operation.
    Redo {
        /// Redo N operations.
        #[arg(short = 'n', long, default_value = "1")]
        steps: usize,

        /// Preview operations without redoing.
        #[arg(long)]
        preview: bool,
    },

    /// Fork an exploration thread from a state.
    Fork {
        /// Name for the fork (creates thread).
        #[arg(long)]
        name: Option<String>,

        /// State to fork from (default: HEAD).
        #[arg(long)]
        from: Option<String>,
    },

    /// Collapse (squash) multiple states into one.
    Collapse(CollapseArgs),

    /// Compare two states.
    Compare {
        /// First state.
        state_a: String,

        /// Second state.
        state_b: String,

        /// Include semantic analysis.
        #[arg(long)]
        semantic: bool,
    },

    /// Manage markers.
    Marker {
        #[command(subcommand)]
        command: MarkerCommands,
    },

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

    /// Show the repo-wide workspace control tower.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommands,
    },

    /// Merge a thread into current thread.
    Merge(MergeArgs),

    /// Resolve merge conflicts.
    Resolve(ResolveArgs),

    /// Verify repository integrity.
    Fsck {
        /// Full check (includes content verification).
        #[arg(long)]
        full: bool,

        /// Run slower graph and signature integrity checks.
        #[arg(long)]
        thorough: bool,

        /// Attempt to repair issues.
        #[arg(long)]
        repair: bool,

        /// Include Git-overlay mirror, mapping, notes, and checkout checks.
        #[arg(long)]
        bridge: bool,
    },

    /// Download objects and refs from remote.
    Fetch {
        /// Remote name or URL.
        remote: Option<String>,

        /// Fetch from all remotes.
        #[arg(long)]
        all: bool,
    },

    /// Push to a remote repository.
    Push(PushArgs),

    /// Pull from a remote repository.
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

    /// Manage stashed changes.
    Stash {
        #[command(subcommand)]
        command: StashCommands,
    },

    /// Customer-issued temporary admin grants for Heddle staff.
    /// Time-bounded and audit-trailed; staff need an active grant to
    /// act on customer resources beyond the operator surface.
    #[cfg(feature = "client")]
    Support {
        #[command(subcommand)]
        command: SupportCommands,
    },

    /// Bridge to other version control systems.
    #[cfg(feature = "git-overlay")]
    #[command(after_help = "\
Examples:
  heddle bridge git status                       # what would import / export look like?
  heddle bridge git import --ref main            # adopt one branch as a full Heddle lane
  heddle bridge git sync                         # bidirectional export + import
  heddle bridge git export ../mirror.git         # write a bare git mirror
")]
    Bridge {
        #[command(subcommand)]
        command: BridgeCommands,
    },

    /// Semantic analysis queries (call-graph hot-spots, churn,
    /// signature-stability surfaces).
    #[cfg(feature = "semantic")]
    Semantic {
        #[command(subcommand)]
        command: SemanticCommands,
    },

    /// Generate shell completion scripts.
    Completion {
        /// Shell to generate completion for.
        shell: String,
    },

    /// Hidden compatibility alias for `maintenance gc`.
    #[command(hide = true)]
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

    /// Hidden compatibility alias for `maintenance index`.
    #[command(hide = true)]
    Index {
        /// Dump the index contents in human-readable format.
        #[arg(long)]
        dump: bool,
    },

    /// Hidden compatibility alias for `maintenance monitor`.
    #[command(hide = true)]
    Monitor {
        /// Print changed paths as well as backend/status summary.
        #[arg(long)]
        paths: bool,

        /// Internal helper mode: serve monitor queries for this repo.
        #[arg(long, hide = true)]
        serve: bool,
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

    /// Object-store maintenance commands.
    ///
    /// `store warm <state>` proactively promotes every blob reachable
    /// from `<state>` into the canonical loose-uncompressed form.
    /// This keeps the hardlink-first materializer on its fast path
    /// after `pack_objects + prune_loose_objects` has consolidated
    /// blobs into packfiles. Useful before fanning out N worktrees
    /// from the same state (e.g. before a `heddle delegate` round).
    Store {
        #[command(subcommand)]
        command: StoreCommands,
    },

    /// Show line-by-line attribution for a file.
    Blame {
        /// File to blame.
        file: String,

        /// State to blame from (default: HEAD).
        #[arg(long)]
        state: Option<String>,

        /// Show concise applicable context before blame output.
        #[arg(long)]
        context: bool,
    },

    /// Binary search for bugs.
    Bisect {
        #[command(subcommand)]
        command: BisectCommands,
    },

    /// Apply specific commits.
    CherryPick {
        /// Commit to cherry-pick.
        commit: String,

        /// Commit message for the cherry-pick.
        #[arg(short = 'm', long)]
        message: Option<String>,

        /// Apply changes to worktree without committing.
        #[arg(long)]
        no_commit: bool,

        /// Discard uncommitted local changes instead of refusing.
        #[arg(long)]
        force: bool,
    },

    /// Clone from remote.
    Clone(CloneArgs),

    /// Rebase current thread onto another.
    Rebase {
        /// Thread to rebase onto.
        thread: Option<String>,

        /// Abort an in-progress rebase.
        #[arg(long)]
        abort: bool,

        /// Continue an in-progress rebase after resolving conflicts.
        #[arg(long = "continue", alias = "cont")]
        cont: bool,

        /// Discard uncommitted local changes instead of refusing.
        #[arg(long)]
        force: bool,
    },

    /// Manage repository hooks.
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },

    /// JSONL bridge for harness session reporting.
    ///
    /// Internal plumbing — invoked by `heddle run` and integration
    /// adapters via `HEDDLE_HARNESS_BRIDGE_*` env vars. Hidden from
    /// `heddle --help` and `heddle help advanced` because it isn't a
    /// user-facing verb. The verb itself still works and is documented
    /// in `docs/HARNESS_ACTOR_INTEGRATION.md`.
    #[command(hide = true)]
    HarnessBridge,

    /// Advanced debugging/provenance commands for Heddle actors attached to threads.
    Actor {
        #[command(subcommand)]
        command: ActorCommands,
    },

    /// Advanced debugging/provenance commands for Heddle execution sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },

    /// Advanced debugging/provenance commands for the hosted local-agent presence relay.
    ///
    /// `heddle presence publish` runs a foreground publisher that streams
    /// agent_start / agent_heartbeat / agent_done events to the configured
    /// hosted server over a bearer-authenticated WebSocket.
    #[cfg(feature = "client")]
    Presence {
        #[command(subcommand)]
        command: PresenceCommands,
    },
}

/// Presence subcommands.
#[cfg(feature = "client")]
#[derive(Clone, Debug, clap::Subcommand)]
pub enum PresenceCommands {
    /// Publish presence events for the given agent session.
    ///
    /// Intended to be launched detached by an orchestrator (or called
    /// manually for debugging). Reads agent metadata from
    /// `.heddle/agents/<session>.toml` and hosted upstream from
    /// `.heddle/config.toml` `[hosted]`. Exits 0 (with a log line) when no
    /// upstream is configured.
    Publish {
        /// Agent session ID (matches `.heddle/agents/<session>.toml`).
        #[arg(long)]
        session: String,

        /// Heartbeat interval in seconds (default 15).
        #[arg(long, default_value = "15")]
        interval_secs: u64,
    },
}

/// Maintenance subcommands.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum MaintenanceCommands {
    /// Inspect repository performance sidecars and repo shape.
    Inspect,

    /// Rebuild repository performance sidecars without changing repository meaning.
    Run,

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

    /// Inspect and debug the worktree index.
    #[command(hide = true)]
    Index {
        /// Dump the index contents in human-readable format.
        #[arg(long)]
        dump: bool,
    },

    /// Inspect the local change monitor state.
    #[command(hide = true)]
    Monitor {
        /// Print changed paths as well as backend/status summary.
        #[arg(long)]
        paths: bool,

        /// Internal helper mode: serve monitor queries for this repo.
        #[arg(long, hide = true)]
        serve: bool,
    },
}

/// Store subcommands.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum StoreCommands {
    /// Promote every reachable blob from `<state>` into the
    /// uncompressed-loose canonical store so the hardlink-first
    /// materializer can `link(2)` directly without paying
    /// `decompress + write` on the first materialize.
    ///
    /// Defaults to HEAD when `<state>` is omitted. Idempotent: a
    /// second call is essentially a no-op (every blob is already
    /// loose+uncompressed).
    Warm {
        /// State specifier (HEAD, thread name, marker, change-id);
        /// defaults to HEAD when omitted.
        state: Option<String>,
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

/// Actor subcommands.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum ActorCommands {
    /// Register a new actor lane (creates a thread + registry entry).
    /// Does not create a filesystem-isolated checkout — for that use
    /// `heddle start <name> --path <dir>`.
    Spawn(ActorSpawnArgs),

    /// List actors known to this repository.
    List(ActorListArgs),

    /// Show the current or selected actor.
    Show(ActorShowArgs),

    /// Explain why Heddle attached the current or selected actor.
    Explain(ActorExplainArgs),

    /// Mark the current or selected actor complete.
    Done(ActorDoneArgs),
}

// `AgentCommands` lives in `commands_agent.rs`. Codex's foundation
// commit added a parallel definition here; deleted during the rebase
// onto main (which had already introduced the file). The reservation
// variants Codex contributed are now folded into the canonical enum in
// `commands_agent.rs`.

/// Session subcommands.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum SessionCommands {
    /// Start a new session.
    Start(SessionStartArgs),

    /// Create a new segment (provider/model change).
    Segment(SessionSegmentArgs),

    /// End the current session.
    End(SessionEndArgs),

    /// Show session details.
    Show(SessionShowArgs),

    /// List all sessions.
    List(SessionListArgs),
}
