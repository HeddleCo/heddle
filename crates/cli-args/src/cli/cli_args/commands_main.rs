// SPDX-License-Identifier: Apache-2.0
//! Top-level CLI commands.

use clap::{Args, Subcommand};

#[cfg(feature = "git-overlay")]
use super::BridgeCommands;
#[cfg(feature = "semantic")]
use super::SemanticCommands;
use super::{
    AgentCommands, BlameArgs, CompletionSubject, ContextCommands, DiscussArgs, EnvCommands,
    HookCommands, ImportArgs, IntegrationCommands, OplogCommands, QueryArgs, RedactCommands,
    RemoteCommands, ReviewCommands, ShellCommands, ThreadCommands, VisibilityCommands,
    commands_args::{
        CloneArgs, DiffArgs, DoctorArgs, IMPORT_VERB, INIT_VERB, InitArgs, LandArgs, LogArgs,
        PullArgs, PushArgs, ReadyArgs, ResolveArgs, RevertArgs, SnapshotArgs, SyncArgs,
        ThreadStartArgs, UndoArgs, WatchArgs,
    },
};
#[cfg(feature = "client")]
use super::{AuthCommands, AuthInviteCommands, ClaimArgs, GrantCommands, PromoteArgs};

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

    #[command(flatten)]
    pub dry_run: super::DryRunArgs,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize Heddle in a directory or existing Git checkout.
    #[command(name = INIT_VERB)]
    Init(InitArgs),

    /// Bring an existing Git repository into Heddle.
    #[command(name = IMPORT_VERB, verbatim_doc_comment)]
    Import(ImportArgs),

    /// Curated, progressive-disclosure help.
    ///
    /// `heddle help` prints the task map. `heddle help --all` prints the
    /// full command tree. `heddle help <topic>` prints the topic page
    /// (e.g. `model`, `advanced`, `git-concepts`). `heddle help
    /// <command path>` falls through to that command's `--help`.
    Help {
        /// Print the full command tree instead of the task map.
        #[arg(long)]
        all: bool,
        /// Topic name (`model`, `advanced`, `git-concepts`, …) or command
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

    /// Run Heddle CI checks. Finds `ci.ts` / `ci.rs` / `ci.go`, compiles if needed, then runs.
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

    /// Integrate into the local target thread; push to publish.
    ///
    /// `land` is the local integration verb: capture outstanding work if needed,
    /// refresh against the target when safe, and land the thread. It fails
    /// closed when conflicts or other blockers exist. Pair it with `ready`
    /// when you want the verdict and next action before landing anything.
    Land(LandArgs),

    /// Check this checkout before local integration.
    ///
    /// `ready` captures outstanding work if needed, checks conflicts,
    /// blockers, freshness, and semantic risk, then marks the thread
    /// ready or blocked and prints the next action. It never lands,
    /// checkpoints, or pushes; use it when you want Heddle's verdict
    /// before integrating the work.
    Ready(ReadyArgs),

    /// Capture a recoverable Heddle step for undo, provenance, and review.
    Capture(SnapshotArgs),

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

    /// Show line-by-line attribution for a tracked file.
    ///
    /// Names the state that last changed each line, with the same
    /// structured principal / agent shape as `log` and `show`.
    /// `heddle query --attribution <path>` remains as the equivalent
    /// query form.
    #[command(after_help = "\
Examples:
  heddle blame src/auth.rs
  heddle blame src/auth.rs --state HEAD
  heddle blame src/auth.rs --context
  heddle blame src/auth.rs --output json
")]
    Blame(BlameArgs),

    /// Open or resolve discussions anchored to code.
    ///
    /// Subcommands: `new`, `reply`, `resolve`, `reopen`, `list`, `show`,
    /// `wait`. `--path` / `--symbol` / `--line` are the code anchor;
    /// `--state` is the historical revision; `--body` / `--file` is the
    /// markdown body.
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
  heddle discuss new --path src/lib.rs --symbol greet --body \"why greet?\"
  heddle discuss new --path src/lib.rs --file why.md
  heddle discuss reply disc-01a0afc6 --body \"second thought\"
  heddle discuss reply disc-01a0afc6 --turn 2 --body \"reply to that turn\"
  heddle discuss resolve <id> --mode by-edit --state HEAD
")]
    Discuss(DiscussArgs),

    /// Structured query over the operation log. Filter by
    /// actor, time window, signal kind, symbol, thread, verbs. Returns
    /// structured results consumable by agents.
    Query(QueryArgs),

    /// Review and approve an exact hosted Thread comparison.
    ///
    /// `show`, `approve`, and `list` default to the current Thread.
    /// `readiness` checks the exact source and target comparison before
    /// hosted landing. Remote selection is explicit, then the configured
    /// default, and fails when neither is available.
    #[command(after_help = "\
Examples:
  heddle review show feature
  heddle review approve feature -m \"Looks good\"
  heddle review list feature
  heddle review revoke <review-id> --thread feature
  heddle review readiness feature --into main
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
    ///
    /// Redaction is path/blob hide inside history. It is not `heddle env`
    /// (runtime secrets; literal `.env` capture is reserved, exit 65) and
    /// not `heddle visibility` (per-state audience, downward-closed).
    Redact {
        #[command(subcommand)]
        command: RedactCommands,
    },

    /// Declare and inspect a state's audience visibility tier.
    ///
    /// `heddle visibility set` binds a tier to a state; `promote` lifts it to
    /// a less-restrictive tier via a superseding record; `show` reports the
    /// effective declared tier; `list` enumerates non-public sidecar records
    /// in this store. Capture binds the inherited
    /// `[review.discussion] default_visibility` automatically (Invariant A)
    /// — these verbs are the explicit operator overrides.
    ///
    /// Private is per-state and downward-closed: a public descendant that
    /// still names private-ancestor blobs is withheld from lesser audiences.
    /// It does not hide one path inside a later public tip. Runtime secrets
    /// belong in `heddle env` (literal `.env` capture is reserved, exit 65);
    /// path-level hide of bytes already in history is `heddle redact`.
    /// See `heddle help visibility`.
    Visibility {
        #[command(subcommand)]
        command: VisibilityCommands,
    },

    /// Run a child with a confidential runtime profile.
    ///
    /// `heddle env run --profile <name> -- <cmd>` asks the local policy
    /// broker to unwrap named slots and injects them into the child
    /// environment only. Values never land in the worktree, the store, or
    /// command JSON. Same-UID callers are cooperative; OS isolation is later.
    ///
    /// Literal `.env` / `.env.local` files are reserved (capture exits 65).
    /// `heddle visibility` does not replace this for secrets beside a public
    /// tip; `heddle redact` stubs a blob already in history.
    #[command(after_help = "\
Examples:
  heddle env list
  heddle env create --name local --from-env DATABASE_URL
  heddle env run --profile local -- printenv DATABASE_URL

Literal `.env` capture is reserved (exit 65). Use this verb for runtime
secrets. `heddle visibility` embargoes a state and its descendants;
`heddle redact` hides a blob already in history. See `heddle help visibility`.
")]
    Env {
        #[command(subcommand)]
        command: EnvCommands,
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

    /// Create or list signup invites (thin alias of `auth invite`).
    ///
    /// Signup-only: this mints an account-creation code. It does not grant
    /// another principal access to a spool. Use `heddle grant` to add a
    /// collaborator.
    #[cfg(feature = "client")]
    #[command(args_conflicts_with_subcommands = true)]
    #[command(after_help = "\
Signup-only. `heddle invite` is the same as `heddle auth invite`.
It does not grant spool access. Add a collaborator with:

  heddle grant create --spool <path|url> --principal <handle> --role writer
")]
    Invite {
        /// Bind the new invite to an email address.
        #[arg(long)]
        email: Option<String>,

        /// Heddle server address. Omit to use the configured default
        /// (`api.heddle.sh` when none is stored).
        #[arg(long, global = true)]
        server: Option<String>,

        #[command(subcommand)]
        command: Option<AuthInviteCommands>,
    },

    /// Grant a principal access to a hosted spool.
    ///
    /// Separate from `heddle auth invite`, which is signup-only. Create,
    /// list, and delete collaborator grants on a spool you can administer.
    /// Agents may grant writer or below; maintainer, admin, and owner stay human-verified.
    #[cfg(feature = "client")]
    Grant {
        #[command(subcommand)]
        command: GrantCommands,
    },

    /// Promote a personal hosted spool to a root-level spool.
    ///
    /// Moves `spool/<your-handle>/<name>` to `spool/<name>` after the server
    /// confirms the root slug is free, the account is claimed/verified, and
    /// you hold an owner grant. Clone a bare name still prefers your personal
    /// copy first.
    #[cfg(feature = "client")]
    Promote(PromoteArgs),

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
  heddle context set --path src/auth.rs --symbol verify --kind invariant -m 'returns false on timing mismatch'
  heddle context get --path src/auth.rs --symbol verify
  heddle context history --path src/auth.rs      # same --path as set, or pass the id
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

    /// Box-scoped network daemon control plane — distinct from
    /// `daemon` (FUSE mounts).
    ///
    /// `heddle netd serve` runs a long-lived async daemon that binds
    /// the machine's single persistent Iroh endpoint on the persisted
    /// device node id and keeps it relay-reachable, so outstanding
    /// claim links keep resolving across restarts. Hosted verbs
    /// (`whoami`, `push`, `pull`, `clone`) reuse that endpoint's warm
    /// weft session when netd is running. Unlike `daemon`, it is not
    /// gated on Linux/FUSE and never idle-exits. `status` reports
    /// liveness and the advertised node id; `stop` asks a running
    /// daemon to close its endpoint and exit.
    Netd {
        #[command(subcommand)]
        command: NetdCommands,
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

    /// Download an existing repository into a local directory.
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

/// Box-scoped network daemon subcommands. See `Commands::Netd`.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum NetdCommands {
    /// Run the foreground network daemon: bind the persistent device
    /// endpoint, keep relays online, hold warm weft sessions for hosted
    /// CLI verbs, and serve same-uid control RPCs. Never idle-exits.
    Serve,

    /// Report network-daemon liveness and the advertised device node
    /// id. No-op success when the daemon isn't running.
    Status,

    /// Ask the running network daemon to close its endpoint and exit.
    Stop,
}
