// SPDX-License-Identifier: Apache-2.0
//! Named argument structs for top-level CLI commands.

/// Arguments for the `init` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle init                                  # initialize the current directory
  heddle init my-project                       # initialize a subdirectory
  heddle init --principal-name 'Ada Lovelace'  # set attribution at init time
")]
pub struct InitArgs {
    /// Directory to initialize (default: current directory).
    pub path: Option<std::path::PathBuf>,

    /// Principal name for attribution.
    #[arg(long)]
    pub principal_name: Option<String>,

    /// Principal email for attribution.
    #[arg(long)]
    pub principal_email: Option<String>,

    /// Install harness integrations after init.
    #[arg(long)]
    pub install_harnesses: Option<String>,

    /// Skip harness integration install prompts.
    #[arg(long)]
    pub no_harness_install: bool,

    /// Preferred install scope (`repo` or `user`).
    #[arg(long, visible_alias = "scope", default_value = "repo")]
    pub harness_install_scope: String,

    /// Overwrite Heddle-managed integration entries when needed.
    #[arg(long)]
    pub harness_install_force: bool,
}

/// Arguments for the `diagnose` command.
#[derive(Clone, Debug, clap::Args)]
pub struct DiagnoseArgs {
    /// Include local timing for the diagnosis read path.
    #[arg(long)]
    pub profile: bool,
}

/// Arguments for the `doctor` command (and its subcommands).
///
/// `heddle doctor` with no subcommand runs the legacy diagnose summary
/// (repository, thread, actor, workspace health). `heddle doctor docs`
/// runs the documentation truthfulness checker — see [`DoctorDocsArgs`]
/// for that surface.
#[derive(Clone, Debug, clap::Args)]
pub struct DoctorArgs {
    /// Include local timing for the diagnosis read path.
    ///
    /// Only honoured when no subcommand is given (i.e. when `heddle
    /// doctor` runs the legacy diagnose summary). Subcommands like
    /// `heddle doctor docs` ignore it.
    #[arg(long, global = false)]
    pub profile: bool,

    #[command(subcommand)]
    pub command: Option<DoctorCommands>,
}

/// `heddle doctor <subcommand>` surface.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum DoctorCommands {
    /// Diff-check markdown documentation against the actual CLI surface.
    ///
    /// Walks every `heddle <verb> [<subverb>] [flags]` invocation in
    /// the requested markdown files and reports any drift: missing
    /// verbs, unknown long flags, or invalid literal values for flags
    /// like `--workspace`, `--scope`, and `--kind`.
    ///
    /// Exits non-zero when any drift is found, so it's safe to run in
    /// CI. Pair with `--json` for structured output. Run on every PR
    /// to prevent the docs from drifting from the CLI again.
    Docs(DoctorDocsArgs),

    /// Drift-check `docs/json-schemas.md` against the registered
    /// schemas.
    ///
    /// Generates the canonical schema for every verb in the schemas
    /// registry, parses every `## heddle <verb> --json` sample in
    /// `docs/json-schemas.md`, and verifies that every key in the
    /// sample is declared in the schema. Exits non-zero on drift.
    /// Pair with `--json` for CI. Run alongside `heddle doctor docs`
    /// on every PR.
    Schemas,
}

/// Arguments for `heddle doctor docs`.
#[derive(Clone, Debug, clap::Args)]
pub struct DoctorDocsArgs {
    /// Markdown file(s) to scan. Repeatable.
    ///
    /// When neither `--path` nor `--all` is given, defaults to
    /// `--all`.
    #[arg(long, value_name = "PATH")]
    pub path: Vec<std::path::PathBuf>,

    /// Scan every tracked `.md` file in the repository.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for the `capture` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle capture -m 'add login route'           # capture the worktree with intent
  heddle capture -m 'wip' --confidence 0.6      # honest confidence on a draft step
  heddle capture --split --into auth -- src/    # move dirty paths into a sibling thread
")]
pub struct SnapshotArgs {
    /// Natural language intent for this recoverable step.
    #[arg(short = 'm', long)]
    pub intent: Option<String>,

    /// Confidence level (0.0-1.0).
    #[arg(long)]
    pub confidence: Option<f32>,

    /// Allow a large or deletion-heavy capture without the safety preflight.
    #[arg(short, long)]
    pub force: bool,

    /// Override HEDDLE_AGENT_PROVIDER.
    #[arg(long)]
    pub agent_provider: Option<String>,

    /// Override HEDDLE_AGENT_MODEL.
    #[arg(long)]
    pub agent_model: Option<String>,

    /// Override HEDDLE_SESSION_ID.
    #[arg(long)]
    pub agent_session: Option<String>,

    /// Override HEDDLE_SESSION_SEGMENT.
    #[arg(long)]
    pub agent_segment: Option<String>,

    /// Override HEDDLE_AGENT_POLICY.
    #[arg(long)]
    pub policy: Option<String>,

    /// Omit policy attribution.
    #[arg(long)]
    pub no_policy: bool,

    /// Omit agent attribution.
    #[arg(long)]
    pub no_agent: bool,

    /// Split selected paths into another thread instead of capturing the whole worktree.
    #[arg(long)]
    pub split: bool,

    /// Target thread when using `--split`.
    #[arg(long, requires = "split")]
    pub into: Option<String>,

    /// Repository-relative path prefix to include when using `--split`.
    #[arg(long = "path", requires = "split", value_name = "PATH")]
    pub paths: Vec<String>,
}

// `CheckpointArgs` lives in `commands_advanced.rs` (canonical
// definition on main). Codex's foundation commit added a parallel
// definition here; deleted during the rebase onto main.

/// Arguments for the `log` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle log                          # walk the current thread
  heddle log --oneline -n 20          # 20 most recent states in compact form
  heddle log --reflog                 # include re-attributed history
  heddle log --paths src/auth.rs      # restrict to states touching a path
")]
pub struct LogArgs {
    /// Starting state (default: HEAD).
    pub state: Option<String>,

    /// Maximum states to show.
    #[arg(short = 'n', long, default_value = "20")]
    pub limit: usize,

    /// Show all states, not just ancestors.
    #[arg(long)]
    pub all: bool,

    /// Show ASCII DAG graph.
    #[arg(long)]
    pub graph: bool,

    /// One state per line.
    #[arg(long)]
    pub oneline: bool,

    /// Show Git-overlay reflog entries instead of Heddle capture history.
    #[arg(long)]
    pub reflog: bool,

    /// Filter by agent model.
    #[arg(long)]
    pub agent: Option<String>,

    /// Show only states that changed the given repository-relative path.
    #[arg(long = "path", value_name = "PATH")]
    pub paths: Vec<String>,

    /// Lower bound: walk back until reaching this state or marker
    /// (exclusive of the bound itself). Accepts a marker name, a
    /// state ID (short or full), or any spec the state resolver
    /// understands. When combined with `--limit`, the bound is
    /// applied first, then the result is trimmed to `--limit`.
    #[arg(long, value_name = "STATE")]
    pub since: Option<String>,
}

/// Arguments for the `retro` command.
///
/// `heddle retro --since <marker-or-state>` summarizes a working
/// session by combining oplog, agent registry, marker, and context
/// annotation reads into one structured payload. Replaces the
/// reconstruct-from-`heddle log` boilerplate agents wrote before.
#[derive(Clone, Debug, clap::Args)]
pub struct RetroArgs {
    /// Lower bound: marker name or state id (short or full). When
    /// omitted, the verb walks back to the most recent `Claude Code
    /// turn`-shaped intent or to one hour ago, whichever is more
    /// recent.
    #[arg(long)]
    pub since: Option<String>,

    /// Include merge entries in the output payload (off by default
    /// because merges are noisy in agent retros).
    #[arg(long)]
    pub include_merges: bool,

    /// Include undo entries in the output payload (off by default
    /// because undos are noisy in agent retros).
    #[arg(long)]
    pub include_undos: bool,

    /// Render full annotation/intent content rather than excerpts.
    /// Aliased as `--full` because the global `-v/--verbose` flag is
    /// already wired as a u8 verbosity counter on `Cli`.
    #[arg(long = "full", alias = "expand")]
    pub full: bool,
}

/// Arguments for the `diff` command.
#[derive(Clone, Debug, clap::Args)]
pub struct DiffArgs {
    /// Base state (default: HEAD).
    pub from: Option<String>,

    /// Target state (default: worktree).
    pub to: Option<String>,

    /// Show semantic changes.
    #[arg(long)]
    pub semantic: bool,

    /// Show diffstat summary only.
    #[arg(long)]
    pub stat: bool,

    /// Show only changed file names.
    #[arg(long)]
    pub name_only: bool,

    /// Number of surrounding context lines to include in each hunk.
    #[arg(short = 'U', long = "unified", default_value_t = 3)]
    pub unified: usize,

    /// Show concise applicable context alongside diff output.
    #[arg(long)]
    pub context: bool,
}

/// Arguments for the `revert` command.
#[derive(Clone, Debug, clap::Args)]
pub struct RevertArgs {
    /// State to revert.
    pub state: String,

    /// Commit message for the revert.
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Apply changes to worktree without committing.
    #[arg(long)]
    pub no_commit: bool,
}

/// Arguments for the `undo` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle undo                # roll back the most recent operation
  heddle undo -n 3           # roll back the last three operations
  heddle undo --list         # preview undoable operations on this thread
  heddle undo --dry-run      # show what would change without applying

Undoable operations:
  - heddle capture           (restores HEAD to the pre-capture parent)
  - heddle merge (non-FF)    (restores HEAD + both thread refs)
  - heddle merge (FF)        (restores HEAD only; the merged-into thread ref
                              stays at the FF target — run `heddle thread
                              switch <name>` to re-attach. Data is never lost.)
  - heddle goto              (restores HEAD to the pre-goto state)
  - heddle thread create/drop/rename
  - heddle marker create/drop
  - heddle redact apply               (with --allow-redact-undo; removes the
                                       redaction record so future materializes
                                       restore the original blob bytes. Refused
                                       when a Purge has destroyed the bytes.)

Not undoable (file a follow-up if you need one):
  - heddle push / heddle fetch        (remote-affecting; out of scope)
  - heddle purge                      (destructive by design; irreversible)
  - cross-thread undo                 (single-thread scope today)
  - redo across CLI invocations       (use `heddle redo` in the same shell)
")]
pub struct UndoArgs {
    /// Undo N operations.
    #[arg(short = 'n', long, default_value = "1")]
    pub steps: usize,

    /// List recent operations without undoing.
    #[arg(long)]
    pub list: bool,

    /// Number of batches to list.
    #[arg(long, default_value = "20")]
    pub depth: usize,

    /// Preview operations without undoing. `--dry-run` is an accepted
    /// alias kept for muscle memory from git/other VCS tooling.
    #[arg(long, visible_alias = "dry-run")]
    pub preview: bool,

    /// Explicit opt-in for undoing a `heddle redact apply`. The inverse
    /// removes the redaction record so subsequent materializes restore
    /// the original blob bytes — i.e. previously-hidden content
    /// becomes readable again. Without this flag, a `heddle undo`
    /// chain that crosses a Redact refuses loudly rather than silently
    /// re-exposing the content. Refused regardless of the flag when
    /// a Purge has destroyed the bytes: Purge is irreversible.
    #[arg(long)]
    pub allow_redact_undo: bool,
}

/// User-facing `--workspace` flag values. Vocabulary is the same as
/// [`crate::cli::commands::repo::ThreadMode`] (and the on-wire
/// `thread.mode` JSON field) so a single name carries through the
/// CLI, the daemon, and the thread record on disk. See
/// `docs/design/clonefile-threads.md` for the rationale.
#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum WorkspaceModeArg {
    /// Let Heddle choose the right mode for this thread (the default):
    /// `materialized` on reflink-capable filesystems, `virtualized`
    /// elsewhere when the mount feature is available, `solid`
    /// otherwise.
    Auto,
    /// Clonefile/reflink the captured tree into a thread directory
    /// (APFS / btrfs / XFS w/ reflinks / bcachefs / ReFS). Real
    /// `read(2)`-able bytes; ~zero disk cost until the agent diverges
    /// blocks. Day-one default on reflink-capable hosts.
    Materialized,
    /// Project the captured tree through a content-addressed
    /// FUSE/FSKit/ProjFS mount. Nothing on disk until the kernel
    /// asks. Requires `heddle` built with the `mount` feature. By
    /// default the mount is owned by the long-lived `heddled` daemon
    /// (survives the CLI exit, shareable across invocations); pass
    /// `--no-daemon` to keep it in this process.
    Virtualized,
    /// Full file copies with no shared extents. Strong isolation;
    /// the right choice on ext4 / NTFS hosts that have neither
    /// reflinks nor a usable mount API.
    Solid,
}

/// Arguments for the `thread start` and top-level `start` commands.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle start feature/auth                       # create or resume a thread
  heddle start feature/auth --workspace materialized     # real checkout on disk
  heddle start scratch --path ../scratch          # place the checkout explicitly
  heddle start fix-flake --task 'fix CI flake'    # attach a task description
")]
pub struct ThreadStartArgs {
    /// Thread name to create or resume.
    pub name: String,

    /// Base state for the thread (default: HEAD).
    #[arg(long)]
    pub from: Option<String>,

    /// Filesystem path for the isolated checkout.
    #[arg(long)]
    pub path: Option<std::path::PathBuf>,

    /// Workspace mode for the thread.
    #[arg(long, value_enum, default_value_t = WorkspaceModeArg::Auto)]
    pub workspace: WorkspaceModeArg,

    /// AI provider name for the registered agent thread.
    #[arg(long)]
    pub agent_provider: Option<String>,

    /// AI model name for the registered agent thread.
    #[arg(long)]
    pub agent_model: Option<String>,

    /// First-class task/goal metadata for the thread.
    #[arg(long)]
    pub task: Option<String>,

    /// Parent thread identifier for delegated child work.
    #[arg(long, hide = true)]
    pub parent_thread: Option<String>,

    /// Internal hint that this thread was started by automation rather than a direct CLI flow.
    #[arg(long, hide = true)]
    pub automated: bool,

    /// Print only the new thread's absolute checkout path to stdout and exit.
    ///
    /// Designed for shell wrappers that want to cd into the new checkout:
    ///   dir=$(heddle start foo --print-cd-path) && cd "$dir"
    /// Skips all other output (no JSON, no styling, no extra lines) so the
    /// stdout is a clean path. Mutually exclusive with `--watch`-style flows.
    #[arg(long, conflicts_with_all = ["agent_provider", "agent_model"])]
    pub print_cd_path: bool,

    /// For `--workspace virtualized`: hand the filesystem mount off to the
    /// long-lived `heddled` daemon (default). The daemon owns the
    /// mount across CLI invocations, so the mount survives `heddle
    /// thread start` exiting. Linux-only; no-op for heavy
    /// workspaces. Pass `--no-daemon` to keep the mount in-process
    /// instead.
    #[arg(
        long,
        overrides_with = "no_daemon",
        action = clap::ArgAction::SetTrue,
        default_value_t = true,
    )]
    pub daemon: bool,

    /// For `--workspace virtualized`: keep the filesystem mount in this CLI
    /// process instead of handing it to the `heddled` daemon. The
    /// mount unmounts when this `heddle thread start` exits — useful
    /// for one-shot inspections, debugging the in-process mount path,
    /// or environments where the daemon can't run.
    #[arg(
        long,
        overrides_with = "daemon",
        action = clap::ArgAction::SetTrue,
    )]
    pub no_daemon: bool,

    /// Redirect cargo's `target/` directory to a workspace-wide shared
    /// path (`.heddle/targets/<workspace-fingerprint>/`) instead of
    /// letting cargo create a per-thread `target/`. Saves multiples of
    /// gigabytes when several materialized threads coexist in a Rust
    /// workspace. Implemented by writing `.cargo/config.toml` inside
    /// the new thread checkout — transparent to any `cargo` invocation
    /// in that directory. Has no effect on light (FUSE-mounted) threads,
    /// and a no-op for repositories without a top-level `Cargo.toml`.
    #[arg(long)]
    pub shared_target: bool,
}

/// Arguments for the `merge` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle merge feature/auth --preview         # structured blockers + recommendation
  heddle merge feature/auth -m 'land auth'    # integrate with a commit message
  heddle merge feature/auth --with-diff       # preview with the resulting diff
  heddle merge feature/auth --semantic        # use semantic merge for code edits
")]
pub struct MergeArgs {
    /// Thread to merge.
    pub thread: String,

    /// Commit message for the merge.
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Apply merge without committing.
    #[arg(long)]
    pub no_commit: bool,

    /// Show semantic integration summary without applying changes.
    #[arg(long)]
    pub preview: bool,

    /// Include the diff (parent ↔ thread tip) in the JSON output.
    /// On `--preview`, this is the diff that *would* land. Without
    /// `--preview` (a real merge) it echoes the diff that just landed.
    #[arg(long = "with-diff")]
    pub with_diff: bool,

    /// When combined with `--with-diff`, return a semantic-aware diff
    /// (function/class-level changes) in addition to line hunks.
    /// Requires building heddle with `--features semantic`.
    #[arg(long)]
    pub semantic: bool,

    /// After a successful (non-preview) merge, also write a git commit
    /// staging the paths the merge introduced. Fails if the worktree
    /// has unrelated uncommitted changes or git is in an unexpected
    /// state (detached HEAD, no `.git`, missing identity). With
    /// `--preview`, the would-be git commit message is included in the
    /// JSON output as `git_commit_preview` without writing anything.
    #[arg(long = "git-commit")]
    pub git_commit: bool,
}

/// Arguments for the `try` command — atomic-ephemeral-thread sugar.
///
/// Implements item 3.1 from the heddle 6→8 plan: spin up an ephemeral
/// thread, run `<cmd>` inside that thread's checkout, capture on
/// success and drop on failure. The parent's working tree is never
/// touched, regardless of whether the command succeeds or fails — the
/// ephemeral thread is a sandbox.
#[derive(Clone, Debug, clap::Args)]
pub struct TryArgs {
    /// Optional thread name. When omitted, defaults to
    /// `try-<short-hash>` derived from the command and a timestamp.
    #[arg(long)]
    pub name: Option<String>,

    /// Workspace mode for the ephemeral thread. Defaults to `heavy`
    /// (a real isolated checkout) so `<cmd>` runs against a proper
    /// filesystem.
    #[arg(long, value_enum, default_value_t = WorkspaceModeArg::Materialized)]
    pub workspace: WorkspaceModeArg,
    /// On zero exit, automatically merge the resulting thread into
    /// the current thread. The merge runs with `--with-diff` so the
    /// JSON payload includes the integrated diff. Default: off (the
    /// command prints a hint pointing at `heddle merge`).
    #[arg(long = "auto-merge")]
    pub auto_merge: bool,

    /// Keep the ephemeral thread on success even if `--auto-merge`
    /// would otherwise drop it after merging. Has no effect on the
    /// failure path (failed attempts are always dropped).
    #[arg(long = "keep-on-success")]
    pub keep_on_success: bool,

    /// The command to run. Everything after `--` lands here. The
    /// first token is the program; the rest are its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// Arguments for the `attempt` command — best-of-N parallel try.
///
/// Implements item 3.2 from the heddle 6→8 plan: spin up N ephemeral
/// threads in parallel, run `<cmd>` in each, optionally rank with an
/// `--evaluate` cmd, and surface a comparison table so the user can
/// pick the winner. Failed attempts are dropped automatically; winning
/// attempts stay around for the user to merge or drop manually.
///
/// `--shared-target` defaults to ON for Rust workspaces because three
/// non-shared parallel cargo builds in a real workspace consume tens
/// of GB of `target/`. Pass `--no-shared-target` to opt out.
#[derive(Clone, Debug, clap::Args)]
pub struct AttemptArgs {
    /// Number of parallel attempts to spawn. Capped at 10 to prevent
    /// fork-bombs on shared CI machines.
    pub n: u32,

    /// Workspace mode for each ephemeral thread. Defaults to `heavy`
    /// (a real isolated checkout) so `<cmd>` runs against a proper
    /// filesystem.
    #[arg(long, value_enum, default_value_t = WorkspaceModeArg::Materialized)]
    pub workspace: WorkspaceModeArg,

    /// Redirect cargo's `target/` for each attempt thread to a shared
    /// workspace-wide path. Default: ON for Rust workspaces (a top-level
    /// `Cargo.toml` is the trigger), OFF otherwise. Pass
    /// `--no-shared-target` to disable.
    #[arg(long = "shared-target", overrides_with = "no_shared_target")]
    pub shared_target: bool,

    /// Disable the auto-on `--shared-target` behaviour for Rust
    /// workspaces. Use only when each attempt genuinely needs an
    /// isolated `target/` (e.g. you're testing the build cache itself).
    #[arg(long = "no-shared-target", overrides_with = "shared_target")]
    pub no_shared_target: bool,

    /// Thread name prefix for the spawned attempts. Final names are
    /// `<prefix>-1`, `<prefix>-2`, …. Defaults to
    /// `attempt-<short-hash>` derived from the command and a timestamp.
    #[arg(long = "name-prefix")]
    pub name_prefix: Option<String>,

    /// Optional secondary command to run inside each attempt thread
    /// after the primary `<cmd>` succeeds. Used for ranking — e.g.
    /// `--evaluate "cargo test"` after a primary that applies a fix.
    /// When absent, ranking uses the primary cmd's exit code, the
    /// resulting diff size, and the wall-clock duration.
    ///
    /// Parsed as a single shell-style string and split on whitespace.
    /// Wrap in quotes when invoking from the shell.
    #[arg(long)]
    pub evaluate: Option<String>,

    /// The command to run inside each attempt thread. Everything after
    /// `--` lands here.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// Arguments for the `run` command.
#[derive(Clone, Debug, clap::Args)]
pub struct RunArgs {
    /// Thread to execute within.
    #[arg(long = "thread")]
    pub thread: Option<String>,

    /// Command to run inside the thread execution root.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// Arguments for the `ready` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ReadyArgs {
    /// Thread to evaluate for integration readiness.
    #[arg(long = "thread")]
    pub thread: Option<String>,

    /// Intent/message to use if `ready` needs to capture outstanding work first.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

/// Arguments for the `sync` command.
#[derive(Clone, Debug, clap::Args)]
pub struct SyncArgs {
    /// Thread to refresh (default: current thread).
    #[arg(long = "thread")]
    pub thread: Option<String>,
}

/// Arguments for the `ship` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ShipArgs {
    /// Thread to capture, integrate, and optionally push (default: current thread).
    #[arg(long = "thread")]
    pub thread: Option<String>,

    /// Intent/message to use if ship needs to capture outstanding work first.
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Push after integration completes.
    #[arg(long)]
    pub push: bool,

    /// Skip push even if defaults would otherwise allow it.
    #[arg(long)]
    pub no_push: bool,

    /// Remote to push to when `--push` is used.
    #[arg(long)]
    pub remote: Option<String>,
}

/// One task entry in `heddle delegate <TASKS>...`. Each entry can be:
///
/// - `"task"` — task label only; agent comes from `--agent-provider` /
///   `--agent-model` if set, otherwise no agent attribution.
/// - `"task:provider:model"` — task label plus a per-task agent
///   override, used to race **different** agents against the same prompt
///   (e.g. `delegate "modulo:anthropic:claude-sonnet-4-5"
///   "modulo:openai:gpt-5-codex" "modulo:custom:opencode"`).
///
/// Parsed lazily by clap via [`parse_delegated_task`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegatedTaskSpec {
    pub task: String,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// clap value parser for `DelegatedTaskSpec`. Splits on `:` left-to-right
/// with at most two splits, so the task label may not contain a literal
/// colon — but task labels are slugified downstream anyway (colons would
/// not survive `slugify`), so this is not a real restriction.
pub fn parse_delegated_task(s: &str) -> Result<DelegatedTaskSpec, String> {
    let mut parts = s.splitn(3, ':');
    let task = parts
        .next()
        .ok_or_else(|| "empty task spec".to_string())?
        .to_string();
    if task.is_empty() {
        return Err("delegated task label may not be empty".to_string());
    }
    let provider = parts
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let model = parts
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    if model.is_some() && provider.is_none() {
        // Shouldn't be reachable given the splitn, but guard anyway —
        // a `task::model` form (empty provider, set model) is ambiguous.
        return Err(format!(
            "delegated task spec {s:?} has a model but no provider; \
             expected `task:provider:model` or `task:provider` or `task`"
        ));
    }
    Ok(DelegatedTaskSpec {
        task,
        provider,
        model,
    })
}

/// Arguments for the `delegate` command.
#[derive(Clone, Debug, clap::Args)]
pub struct DelegateArgs {
    /// Child task labels to create under the current parent thread.
    ///
    /// Each entry is either `task` or `task:provider:model`. Use the
    /// latter form to race different agents on the same prompt:
    ///   heddle delegate "modulo:anthropic:claude-sonnet-4-5" \
    ///                   "modulo:openai:gpt-5-codex" \
    ///                   "modulo:custom:opencode"
    #[arg(required = true, value_parser = parse_delegated_task)]
    pub tasks: Vec<DelegatedTaskSpec>,

    /// Parent thread to delegate from (default: current thread).
    #[arg(long)]
    pub parent: Option<String>,

    /// Workspace mode for delegated child threads.
    #[arg(long, value_enum)]
    pub workspace: Option<WorkspaceModeArg>,

    /// Directory under which materialized child workspaces should be created.
    #[arg(long)]
    pub path_prefix: Option<std::path::PathBuf>,

    /// Default AI provider for delegated children (overridden per-task
    /// when a `task:provider:model` form is used).
    #[arg(long)]
    pub agent_provider: Option<String>,

    /// Default AI model for delegated children (overridden per-task
    /// when a `task:provider:model` form is used).
    #[arg(long)]
    pub agent_model: Option<String>,
}

/// Arguments for `thread show`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadShowArgs {
    /// Thread identifier. Defaults to the current thread when omitted.
    pub thread: Option<String>,

    /// Continuously refresh thread status.
    #[arg(long)]
    pub watch: bool,

    /// Internal helper for tests: stop after N watch updates.
    #[arg(long, hide = true)]
    pub watch_iterations: Option<usize>,

    /// Internal helper for tests: polling interval in milliseconds.
    #[arg(long, hide = true)]
    pub watch_interval_ms: Option<u64>,
}

/// Arguments for `thread captures`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadCapturesArgs {
    /// Thread identifier. Defaults to the current thread when omitted.
    pub thread: Option<String>,

    /// Maximum captures to show.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

/// Arguments for commands that take a thread identifier. Omitting the
/// positional resolves to the current thread when one can be inferred
/// from the working checkout.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadNameArgs {
    /// Thread identifier. Defaults to the current thread when omitted.
    pub thread: Option<String>,
}

/// Arguments for `thread rename`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadRenameArgs {
    /// Existing thread identifier.
    pub old: String,

    /// New thread identifier.
    pub new: String,
}

/// Arguments for `thread promote`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadPromoteArgs {
    /// Thread identifier.
    pub thread: String,

    /// Materialized checkout path.
    #[arg(long)]
    pub path: Option<std::path::PathBuf>,
}

/// Arguments for `thread move`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadMoveArgs {
    /// Source thread identifier.
    pub from: String,

    /// Destination thread identifier.
    pub to: String,

    /// Repository-relative path prefix to move.
    #[arg(long = "path", required = true, value_name = "PATH")]
    pub paths: Vec<String>,

    /// Intent/message for the snapshots created by the move.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

/// Arguments for `thread absorb`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadAbsorbArgs {
    /// Child thread to absorb.
    pub thread: String,

    /// Parent thread to absorb into (default: the thread's recorded parent).
    #[arg(long)]
    pub into: Option<String>,

    /// Commit message for the absorb merge.
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Show the absorb preview without applying it.
    #[arg(long)]
    pub preview: bool,
}

/// Arguments for `thread resolve`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadResolveArgs {
    /// Thread identifier.
    pub thread: String,
}

/// Arguments for `thread drop`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadDropArgs {
    /// Thread identifier.
    pub thread: String,

    /// Also delete the attached thread ref.
    #[arg(long)]
    pub delete_thread: bool,
}

/// Arguments for `thread approve` — record an approval for a
/// `<source> -> <target>` merge against the source thread's
/// current state.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadApproveArgs {
    /// Source thread identifier (the change set being merged).
    pub source: String,

    /// Target thread identifier (where the merge would land).
    pub target: String,

    /// Optional human note attached to the approval.
    #[arg(long)]
    pub note: Option<String>,

    /// Hosted remote name (default: `origin`).
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

/// Arguments for `thread approvals` — list every approval recorded
/// for `<source> -> <target>`.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadApprovalsArgs {
    pub source: String,
    pub target: String,
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

/// Arguments for `thread revoke-approval` — remove a recorded
/// approval by id.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadRevokeApprovalArgs {
    /// UUID of the approval row to revoke.
    pub id: String,
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

/// Arguments for `thread check-merge` — query the merge gate
/// without recording anything. Returns the unmet requirements.
#[derive(Clone, Debug, clap::Args)]
pub struct ThreadCheckMergeArgs {
    pub source: String,
    pub target: String,

    /// 'merge' (default), 'force_push', or 'complete'.
    #[arg(long, default_value = "merge")]
    pub gated_action: String,

    /// File paths the diff touches, repeat or comma-separate. Empty =
    /// "we don't know" (every path-conditional policy fires).
    #[arg(long = "path", value_delimiter = ',')]
    pub changed_paths: Vec<String>,

    #[arg(long, default_value = "origin")]
    pub remote: String,
}

/// Arguments for the `collapse` command.
#[derive(Clone, Debug, clap::Args)]
pub struct CollapseArgs {
    /// States to collapse.
    #[arg(required = true)]
    pub states: Vec<String>,

    /// Intent/name for the resulting state.
    #[arg(long)]
    pub into: String,

    /// Confidence for the resulting state (0.0-1.0).
    #[arg(long)]
    pub confidence: Option<f32>,
}

/// Arguments for the `resolve` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ResolveArgs {
    /// File to resolve.
    pub path: Option<String>,

    /// Resolve all conflicts.
    #[arg(long)]
    pub all: bool,

    /// List unresolved conflicts.
    #[arg(long)]
    pub list: bool,

    /// Use our version (current thread).
    #[arg(long, conflicts_with = "theirs")]
    pub ours: bool,

    /// Use their version (merged thread).
    #[arg(long, conflicts_with = "ours")]
    pub theirs: bool,

    /// Abort the merge.
    #[arg(long)]
    pub abort: bool,
}

/// The `(remote, thread)` pair shared by every command that targets a
/// remote: `push`, `pull`, `fetch`. Flattening once keeps the field
/// docstrings consistent and lets a future "add `--token` to all three"
/// land in a single place.
#[derive(Clone, Debug, clap::Args)]
pub struct RemoteOperationArgs {
    /// Remote address (host:port).
    pub remote: Option<String>,

    /// Thread to act on.
    #[arg(short, long)]
    pub thread: Option<String>,
}

/// Arguments for the `push` command.
#[derive(Clone, Debug, clap::Args)]
pub struct PushArgs {
    #[command(flatten)]
    pub remote_op: RemoteOperationArgs,

    /// State to push (default: HEAD).
    #[arg(short, long)]
    pub state: Option<String>,

    /// Force push.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for the `pull` command.
#[derive(Clone, Debug, clap::Args)]
pub struct PullArgs {
    #[command(flatten)]
    pub remote_op: RemoteOperationArgs,

    /// Local thread to update.
    #[arg(short, long)]
    pub local_thread: Option<String>,

    /// Leave blob content absent by design and hydrate it explicitly later.
    #[arg(long)]
    pub lazy: bool,
}

/// Arguments for the `clone` command.
#[derive(Clone, Debug, clap::Args)]
pub struct CloneArgs {
    /// Remote repository path.
    pub remote: String,

    /// Local directory to clone into.
    pub local: String,

    /// Thread to check out after cloning.
    #[arg(long)]
    pub thread: Option<String>,

    /// Create a shallow clone with the specified depth. `0` means full history.
    #[arg(long)]
    pub depth: Option<u32>,

    /// Leave blob content absent by design and hydrate it explicitly later.
    #[arg(long)]
    pub lazy: bool,

    /// Partial-clone filter spec. Currently only `blob:none` is supported,
    /// which is a synonym for `--lazy` (skip blob content; hydrate on demand
    /// later). Other git-style filters such as `tree:0` or `blob:limit=…`
    /// are rejected.
    #[arg(long, value_name = "SPEC", value_parser = parse_clone_filter_spec)]
    pub filter: Option<String>,
}

fn parse_clone_filter_spec(s: &str) -> Result<String, String> {
    match s {
        "blob:none" => Ok(s.to_string()),
        other => Err(format!(
            "unsupported --filter spec `{other}`; only `blob:none` is supported today"
        )),
    }
}

/// Arguments for the `session start` command.
#[derive(Clone, Debug, clap::Args)]
pub struct SessionStartArgs {
    /// Provider name (e.g., "anthropic", "openai").
    #[arg(long)]
    pub provider: String,

    /// Model identifier (e.g., "claude-opus-4").
    #[arg(long)]
    pub model: String,

    /// Policy or prompt template ID.
    #[arg(long)]
    pub policy: Option<String>,
}

/// Arguments for the `session segment` command.
#[derive(Clone, Debug, clap::Args)]
pub struct SessionSegmentArgs {
    /// Provider name (e.g., "anthropic", "openai").
    #[arg(long)]
    pub provider: String,

    /// Model identifier (e.g., "claude-opus-4").
    #[arg(long)]
    pub model: String,

    /// Policy or prompt template ID.
    #[arg(long)]
    pub policy: Option<String>,
}

/// Arguments for the `session end` command.
#[derive(Clone, Debug, clap::Args)]
pub struct SessionEndArgs {
    /// Session ID to end (default: current session).
    pub session_id: Option<String>,
}

/// Arguments for the `session show` command.
#[derive(Clone, Debug, clap::Args)]
pub struct SessionShowArgs {
    /// Session ID to show (default: current session).
    pub session_id: Option<String>,
}

/// Arguments for the `session list` command.
#[derive(Clone, Debug, clap::Args)]
pub struct SessionListArgs {
    /// Show only active sessions.
    #[arg(long)]
    pub active: bool,
}

/// Arguments for the `worktree add` command.
#[derive(Clone, Debug, clap::Args)]
pub struct WorktreeAddArgs {
    /// Path to the new agent checkout directory.
    pub path: std::path::PathBuf,

    /// Thread name for the agent (created if absent, default: HEAD thread).
    #[arg(long)]
    pub thread: Option<String>,

    /// Base state to materialize (default: HEAD).
    #[arg(long)]
    pub from: Option<String>,
}

/// Arguments for the `worktree remove` command.
#[derive(Clone, Debug, clap::Args)]
pub struct WorktreeRemoveArgs {
    /// Path to the isolated checkout directory to remove.
    pub path: std::path::PathBuf,

    /// Also delete the associated thread ref, if this checkout is attached.
    #[arg(long)]
    pub delete_thread: bool,
}

/// Arguments for the `actor spawn` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ActorSpawnArgs {
    /// Thread name for the actor (auto-generated if not specified).
    #[arg(long)]
    pub thread: Option<String>,

    /// AI provider name (e.g. `anthropic`).
    #[arg(long)]
    pub provider: Option<String>,

    /// AI model identifier (e.g. `claude-sonnet-4-6`).
    #[arg(long)]
    pub model: Option<String>,
}

/// Arguments for the `actor list` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ActorListArgs {
    /// Show only active actors.
    #[arg(long)]
    pub active: bool,
}

/// Arguments for the `actor show` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ActorShowArgs {
    /// Session ID to show (default: current thread actor).
    pub session: Option<String>,
}

/// Arguments for the `actor explain` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ActorExplainArgs {
    /// Session ID to explain (default: current thread actor).
    pub session: Option<String>,
}

/// Arguments for the `actor done` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ActorDoneArgs {
    /// Session ID to mark as complete (default: current thread actor).
    #[arg(long)]
    pub session: Option<String>,
}

/// Arguments for `agent reserve`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentReserveArgs {
    /// Thread to reserve.
    #[arg(long)]
    pub thread: String,

    /// Anchor state spec (default: current HEAD).
    #[arg(long)]
    pub anchor: Option<String>,

    /// Optional task description.
    #[arg(long)]
    pub task: Option<String>,

    /// Bind the reservation's liveness to an external process pid
    /// instead of this one-shot CLI invocation's pid.
    ///
    /// `heddle agent reserve` exits as soon as the reservation is
    /// recorded, so its own pid is dead by the time another agent
    /// checks liveness — that means the dead-pid reaper would
    /// immediately recycle the reservation. With `--hold-for-pid`
    /// the orchestrator passes its own (long-lived) pid; the
    /// reservation lives as long as that process does, and a SIGKILL
    /// or normal exit on the orchestrator triggers automatic reap.
    ///
    /// This is the daemon-ownership pattern without shipping a daemon.
    #[arg(long, value_name = "PID")]
    pub hold_for_pid: Option<u32>,
}

/// Arguments for `agent heartbeat`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentHeartbeatArgs {
    /// Agent session id.
    #[arg(long)]
    pub session: String,
}

/// Arguments for `agent release`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentReleaseArgs {
    /// Agent session id.
    #[arg(long)]
    pub session: String,

    /// Terminal status to record.
    #[arg(long, default_value = "complete")]
    pub status: AgentReleaseStatusArg,
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum AgentReleaseStatusArg {
    Complete,
    Abandoned,
}

/// Arguments for `agent list`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentApiListArgs {
    /// Filter by thread.
    #[arg(long)]
    pub thread: Option<String>,

    /// Show only active reservations.
    #[arg(long)]
    pub alive_only: bool,
}

/// Arguments for `agent capture`. Mirrors `heddle capture` with an
/// extra `--session` guard so an orchestrator can prove it owns the
/// thread before writing.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentCaptureArgs {
    /// Agent session id obtained from `agent reserve`.
    #[arg(long)]
    pub session: String,

    /// Capture intent / commit message.
    #[arg(long, short = 'm', alias = "intent")]
    pub message: Option<String>,

    /// Honest confidence estimate (0.0–1.0).
    #[arg(long)]
    pub confidence: Option<f32>,
}

/// Arguments for `agent ready`. Mirrors `heddle ready` with the same
/// `--session` guard.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentReadyArgs {
    /// Agent session id obtained from `agent reserve`.
    #[arg(long)]
    pub session: String,

    /// Optional summary message.
    #[arg(long, short = 'm')]
    pub message: Option<String>,
}

/// Arguments for the `watch` command.
///
/// Streams live oplog activity (snapshots, merges, thread create/update,
/// markers, etc.) as it happens. Default behavior tails forever and exits
/// on Ctrl-C. `--since 5m` replays the last N before tailing live;
/// `--filter` restricts output to the named kinds; `--json` emits one
/// JSON object per line for piping to `jq`.
#[derive(Clone, Debug, clap::Args)]
pub struct WatchArgs {
    /// Replay events from this duration ago (e.g. `30s`, `5m`, `1h`,
    /// `2d`) before tailing live. When unset, only new events are
    /// emitted.
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,

    /// Comma-separated event kinds to include
    /// (`snapshot,merge,thread_create,thread_update,thread_delete,
    /// fork,collapse,goto,marker_create,marker_delete`).
    #[arg(long, value_name = "KINDS")]
    pub filter: Option<String>,

    /// Emit one JSON object per line instead of human-readable text.
    /// Use this to pipe to `jq` or downstream tooling. Note: the
    /// global `--json` flag also enables JSON mode.
    #[arg(long)]
    pub json: bool,

    /// Internal helper for tests: stop after the oplog file produces
    /// this many modify events (still drains pending entries first).
    #[arg(long, hide = true)]
    pub max_iterations: Option<usize>,

    /// Internal helper for tests: poll interval in milliseconds for
    /// the `notify` watcher's debounce check (default 200ms).
    #[arg(long, hide = true)]
    pub poll_interval_ms: Option<u64>,
}

// `AgentCaptureArgs` and `AgentReadyArgs` defined earlier in this
// file. A second copy was left here by the rebase (the workstreams
// commit added them twice when the cherry-pick had lost the
// originals and we re-added them mid-rebase). Removed.

#[cfg(test)]
mod clone_filter_tests {
    use clap::Parser;

    use crate::cli::{Cli, CloneArgs, Commands};

    fn parse_clone(extra: &[&str]) -> Result<CloneArgs, clap::Error> {
        let mut argv: Vec<&str> = vec!["heddle", "clone", "remote", "local"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv)?;
        match cli.command {
            Commands::Clone(args) => Ok(args),
            _ => panic!("expected Commands::Clone"),
        }
    }

    #[test]
    fn parses_clone_filter_blob_none() {
        let args = parse_clone(&["--filter", "blob:none"]).expect("parse --filter blob:none");
        assert_eq!(args.filter.as_deref(), Some("blob:none"));
        assert!(!args.lazy);
    }

    #[test]
    fn rejects_unknown_filter_spec() {
        let err = parse_clone(&["--filter", "tree:0"])
            .expect_err("unknown --filter spec should fail to parse");
        let msg = err.to_string();
        assert!(
            msg.contains("tree:0") && msg.contains("blob:none"),
            "error should name the bad spec and the supported one: {msg}"
        );
    }
}
