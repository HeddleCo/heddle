// SPDX-License-Identifier: Apache-2.0
//! Top-level CLI commands.

use clap::{Args, Subcommand};

#[cfg(feature = "git-overlay")]
use super::BridgeCommands;
#[cfg(feature = "semantic")]
use super::SemanticCommands;
use super::{
    AgentCommands, CompletionSubject, ContextCommands, DiscussCommands, HookCommands,
    IntegrationCommands, OplogCommands, QueryArgs, RedactCommands, RemoteCommands, ReviewCommands,
    ShellCommands, ThreadCommands, VisibilityCommands,
    commands_args::{
        AdoptArgs, CloneArgs, CommitArgs, DiffArgs, DoctorArgs, INIT_VERB, InitArgs, LandArgs,
        LogArgs, PullArgs, PushArgs, ReadyArgs, ResolveArgs, RevertArgs, SnapshotArgs, SyncArgs,
        ThreadStartArgs, UndoArgs, WatchArgs,
    },
};
#[cfg(feature = "client")]
use super::{AuthCommands, ClaimArgs};

#[derive(Clone, Debug, Args)]
pub struct FsckArgs {
    /// Full check (includes content verification).
    #[arg(long)]
    pub full: bool,

    /// Run slower graph and signature integrity checks.
    #[arg(long)]
    pub thorough: bool,

    /// Verify offline authorship identity and review-signature chains.
    #[arg(long, requires = "thorough")]
    pub provenance: bool,

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
    #[command(name = INIT_VERB)]
    Init(InitArgs),

    /// Adopt Git history into Heddle-native source authority.
    ///
    /// Git Overlay is the normal existing-Git mode: Git keeps source objects,
    /// refs, index, and worktree state while Heddle stores metadata in
    /// `.heddle`. `adopt` imports history and moves source authority to Heddle.
    Adopt(AdoptArgs),

    /// Curated, progressive-disclosure help.
    ///
    /// `heddle help` prints the locked everyday verbs. `heddle help
    /// <topic>` prints the topic page (e.g. `model`, `daemon`,
    /// `signals`, `git-concepts`). `heddle help <command path>` falls
    /// through to that command's `--help` so the printer never
    /// duplicates clap's per-verb derivation.
    Help {
        /// Topic name (`model`, `daemon`, `signals`, …) or command
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
    Verify {
        /// Verify each state's offline authorship and review-signature chain.
        #[arg(long)]
        provenance: bool,
    },

    /// Explain repository health, or run targeted doctor checks.
    ///
    /// `heddle doctor` (no subcommand) reports repository health and
    /// the next recovery step. `heddle doctor docs` diff-checks markdown
    /// documentation against
    /// the actual CLI surface and exits non-zero on drift — wire it
    /// into CI to stop docs from going stale.
    Doctor(DoctorArgs),

    /// Create or resume an isolated thread for focused work.
    Start(ThreadStartArgs),

    /// Run Heddle CI checks from `.heddle/treadle.definition.bin` (SDK compile output).
    #[cfg(feature = "ci")]
    Ci {
        #[command(subcommand)]
        command: super::CiCommands,
    },

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

    /// Write captured source history to `.git` in Git Overlay.
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

    /// Show state details.
    Show {
        /// State by physical state ID, logical change ID, or unambiguous prefix.
        /// Defaults to HEAD.
        state: Option<String>,
    },

    /// Show what changed in the worktree, a thread, or two states.
    Diff(DiffArgs),

    /// Open or resolve discussions anchored to symbols.
    ///
    /// Open a discussion against a symbol; append turns;
    /// resolve by edit or dismiss. Anchors
    /// travel across renames and cross-file moves on subsequent
    /// state mutations.
    ///
    /// Native Heddle only. Discussions live in `.heddle` and travel
    /// over `heddle push` / `heddle pull` to a Heddle remote. They are
    /// not projected into Git, so `git clone` does not carry them; in
    /// Git Overlay mode they are local to that working copy.
    #[command(after_help = "\
Scope:
  Native Heddle only. Discussions are stored in `.heddle` and move over
  `heddle push` / `heddle pull`. Git does not carry them: a `git clone` of a
  Git Overlay repository arrives with no discussions and no Heddle store.

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
  heddle review show HEAD --base last-turn               # review this agent peer's turn
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
    /// redact purge` afterward physically removes the bytes. Both are signed,
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

    /// Print a tab-completion script for bash, zsh, or fish.
    ///
    /// With no shell, prints install lines. With `bash`, `zsh`, or `fish`,
    /// emits the same script as `heddle shell completion`.
    Completions {
        /// Shell to generate completion for: bash, zsh, or fish.
        #[arg(value_name = "SHELL")]
        shell: Option<String>,
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

    /// Explicit interoperability with other version-control formats.
    #[cfg(feature = "git-overlay")]
    Bridge {
        #[command(subcommand)]
        command: BridgeCommands,
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

    /// Offer this agent account for a human to claim.
    ///
    /// Prints a short-lived bearer link, then keeps the agent's Iroh endpoint
    /// online until the human finishes, the offer expires, or Ctrl-C stops it.
    #[cfg(feature = "client")]
    #[command(after_help = "\
Examples:
  heddle claim
  heddle claim --timeout 30m
  heddle claim --server weft.example --web-origin https://heddle.example
")]
    Claim(ClaimArgs),

    /// Report the capture actor, then hosted auth.
    ///
    /// The capture actor is who the next capture is attributed to
    /// (`user_config`, `init --principal-*`, or `HEDDLE_PRINCIPAL_*`).
    /// Hosted auth is whether this machine has a server credential.
    /// These are different objects. `heddle auth login` does not set the
    /// local actor. `whoami` only reads; it never attaches a credential.
    #[cfg(feature = "client")]
    #[command(after_help = "\
The capture actor and hosted auth are different objects:
  capture actor  who the next capture is attributed to
                 (user_config, init --principal-*, or HEDDLE_PRINCIPAL_*)
  hosted auth    whether this machine has a credential for the server
                 (heddle auth login). whoami never attaches a credential.

Examples:
  heddle whoami                       # capture actor first, then hosted auth
  heddle whoami --output json         # machine-readable, stable output_kind shape
  heddle whoami --server api.heddle.sh")]
    Whoami {
        /// Heddle server address (defaults to the configured server).
        #[arg(long)]
        server: Option<String>,
    },

    /// Manage code context annotations.
    ///
    /// Native Heddle only. Annotations live in `.heddle`, and travel
    /// over `heddle push` / `heddle pull` to a Heddle remote. They are
    /// deliberately not projected into Git — not into `refs/notes/*`,
    /// not into a tracked file — so `git push` and `git clone` do not
    /// carry them. In Git Overlay mode annotations still work and are
    /// still useful; they are simply local to that working copy.
    #[command(after_help = "\
Scope:
  Native Heddle only. Annotations are stored in `.heddle` and move over
  `heddle push` / `heddle pull`. Git does not carry them: a `git clone` of a
  Git Overlay repository arrives with no annotations and no Heddle store.

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

    /// Agent reservation and one-shot orchestration API.
    ///
    /// `heddle agent reserve|capture|ready|release|list|heartbeat` is the stable
    /// JSON contract orchestrators use to coordinate parallel
    /// writers. `heddle daemon` remains the distinct FUSE mount control plane.
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
    /// Verify repository integrity or explicitly repair one surface.
    Fsck(FsckArgs),

    /// Inspect repository performance sidecars and repo shape.
    Inspect,

    /// Refresh repository performance sidecars without changing repository meaning.
    Refresh,

    /// Repack native objects now through the resource-controlled scheduler.
    Repack,

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

    /// Inspect and repair the operation log.
    ///
    /// `heddle maintenance oplog recover` explicitly salvages a truncated or
    /// torn oplog, reporting what was recovered — the operator-facing
    /// entrypoint over the same recovery the everyday read path runs
    /// automatically.
    Oplog {
        #[command(subcommand)]
        command: OplogCommands,
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
