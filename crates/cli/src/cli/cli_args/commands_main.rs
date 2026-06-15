// SPDX-License-Identifier: Apache-2.0
//! Top-level CLI commands.

use clap::Subcommand;

#[cfg(feature = "git-overlay")]
use super::BridgeCommands;
#[cfg(feature = "semantic")]
use super::SemanticCommands;
use super::{
    AgentCommands, CheckpointArgs, CompletionSubject, ContextCommands, DiscussCommands,
    HookCommands, IntegrationCommands, OplogCommands, QueryArgs, RedactCommands, RemoteCommands,
    ReviewCommands, ShellCommands, StashCommands, ThreadCommands, TransactionCommands,
    VisibilityCommands,
    commands_args::{
        ActorDoneArgs, ActorExplainArgs, ActorListArgs, ActorShowArgs, ActorSpawnArgs, AdoptArgs,
        CloneArgs, CollapseArgs, CommitArgs, DiffArgs, DoctorArgs, ExpandArgs, InitArgs, LandArgs,
        LogArgs, MergeArgs, PullArgs, PushArgs, ReadyArgs, ResolveArgs, RetroArgs, RevertArgs,
        RunArgs, SessionEndArgs, SessionListArgs, SessionSegmentArgs, SessionShowArgs,
        SessionStartArgs, SnapshotArgs, SwitchArgs, SyncArgs, ThreadStartArgs, TryArgs, UndoArgs,
        WatchArgs,
    },
};
#[cfg(feature = "client")]
use super::{AuthCommands, SupportCommands};

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new Heddle repository.
    Init(InitArgs),

    /// Adopt the current Git repository into Heddle.
    ///
    /// Initializes Heddle sidecar data if needed and imports Git history
    /// without modifying existing Git worktree changes. Use `--ref` to
    /// adopt selected branches or tags; omit it to import all local refs.
    #[command(visible_alias = "import")]
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

    /// Show the low-friction Git-overlay workflow.
    #[cfg(feature = "git-overlay")]
    GitOverlay,

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
    /// is the joined subcommand path — e.g. `status`, `log`, `bridge
    /// git status`, `marker list`.
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

    /// Land a ready thread and optionally publish it.
    ///
    /// `land` is the local integration verb: capture outstanding work if needed,
    /// refresh against the target when safe, land the thread, write the
    /// Git checkpoint, and optionally push. It fails closed when
    /// conflicts or other blockers exist. Pair it with `ready` when you
    /// want the verdict and next action before landing anything.
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

    /// Save current work as one Heddle change, plus a Git checkpoint in Git-overlay repos.
    Commit(CommitArgs),

    /// Commit the current captured work to the Git-overlay branch/index.
    Checkpoint(CheckpointArgs),

    /// Show state history.
    ///
    /// By default, when a thread name is given (e.g. `heddle log master`),
    /// the walk is *first-parent only* — equivalent to `git log
    /// --first-parent <branch>`. To see every ancestor reachable through
    /// merge commits, pass `--graph` (which renders the full DAG) or
    /// `--all` (which lists every state regardless of ancestry).
    #[command(visible_alias = "history")]
    Log(LogArgs),

    /// Show state details.
    Show {
        /// State by change ID or hash prefix. Defaults to HEAD when omitted.
        state: Option<String>,
    },

    /// Summarize a working session.
    ///
    /// Combines oplog, agent registry, marker, and context-annotation
    /// reads into one structured payload — agent-readable retro of
    /// captures, signals, and notable events since `--since`. Replaces
    /// the reconstruct-from-`heddle log` boilerplate.
    Retro(RetroArgs),

    /// Remove untracked files from worktree.
    Clean {
        /// Actually remove files (required for safety).
        #[arg(short, long)]
        force: bool,

        /// Only show what would be removed.
        #[arg(long)]
        dry_run: bool,
    },

    /// Show what changed in the worktree, a thread, or two states.
    Diff(DiffArgs),

    /// Git-compatible alias for `heddle thread switch`.
    Switch(SwitchArgs),

    /// Open or resolve discussions anchored to symbols.
    ///
    /// Open a discussion against a symbol; append turns;
    /// resolve into an annotation, by edit, or dismissed. Anchors
    /// travel across renames and cross-file moves on subsequent
    /// state mutations.
    #[command(after_help = "\
Examples:
  heddle discuss open src/auth.rs verify 'Should this reject expired tokens?'  # anchor a discussion
  heddle discuss append <id> 'switched to argon2'          # add a turn
  heddle discuss resolve <id> --mode into-annotation --annotation-kind rationale --annotation-content 'Kept for compatibility'
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

    /// Preview or land a thread into the current thread.
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

    /// Inspect and repair the operation log.
    ///
    /// `heddle oplog recover` explicitly salvages a truncated or torn oplog,
    /// reporting what was recovered — the operator-facing entrypoint over the
    /// same recovery the everyday read path runs automatically.
    Oplog {
        #[command(subcommand)]
        command: OplogCommands,
    },

    /// Download objects and refs from remote.
    ///
    /// In Git-overlay mode this fetches branches and refs/notes/heddle, not Git tags.
    Fetch {
        /// Remote name or URL.
        remote: Option<String>,

        /// Fetch from all remotes.
        #[arg(long)]
        all: bool,
    },

    /// Push to a remote repository.
    ///
    /// In Git-overlay mode, push writes plain Git refs the remote's users can
    /// inspect with `git ls-remote`: each Heddle thread's state goes to
    /// `refs/heads/<thread>`, Heddle metadata (state identity carried as Git
    /// notes) goes to `refs/notes/heddle`, and with `--all-threads` Git tags go
    /// to `refs/tags/<tag>`. JSON output lists the refs actually written this
    /// invocation in `refs_written`.
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
