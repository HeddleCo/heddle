// SPDX-License-Identifier: Apache-2.0
//! Named argument structs for top-level CLI commands.

#[cfg(feature = "git-overlay")]
use super::commands_git_projection::SyncCommands;

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

    /// Skip harness integration installation during init.
    #[arg(long)]
    pub no_harness_install: bool,

    /// Preferred install scope (`repo` or `user`).
    #[arg(long, visible_alias = "scope", default_value = "repo")]
    pub harness_install_scope: String,

    /// Overwrite Heddle-managed integration entries when needed.
    #[arg(long)]
    pub harness_install_force: bool,
}

/// Arguments for the `adopt` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle adopt                                # convert all local Git refs into native Heddle storage
  heddle adopt --ref main                     # convert one branch or tag
  heddle adopt ../repo --ref main --ref v1.0  # convert selected refs in another repo

Adoption converts Git history metadata into Heddle-native storage without modifying existing Git worktree changes.
")]
pub struct AdoptArgs {
    /// Git repository to convert into Heddle-native storage (default: current directory).
    pub path: Option<std::path::PathBuf>,

    /// Git branch or tag to convert. Repeat to convert selected refs; omit to convert all refs.
    #[arg(long = "ref", value_name = "REF")]
    pub refs: Vec<String>,
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
    /// CI. Pair with `--output json` for structured output. Run on every PR
    /// to prevent the docs from drifting from the CLI again.
    Docs(DoctorDocsArgs),

    /// Drift-check `docs/json-schemas.md` against the registered
    /// schemas.
    ///
    /// Generates the canonical schema for every verb in the schemas
    /// registry, parses every `## heddle <verb> --output json` sample in
    /// `docs/json-schemas.md`, and verifies that every key in the
    /// sample is declared in the schema. Exits non-zero on drift.
    /// Pair with `--output json` for CI. Run alongside `heddle doctor docs`
    /// on every PR.
    Schemas(DoctorSchemasArgs),
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

/// Arguments for `heddle doctor schemas`.
#[derive(Clone, Debug, clap::Args)]
pub struct DoctorSchemasArgs {
    /// Refresh the generated command-contract coverage sample in
    /// `docs/json-schemas.md`, then run the normal schema drift check.
    #[arg(long)]
    pub update_docs: bool,
}

fn parse_confidence(s: &str) -> Result<f32, String> {
    let value = s
        .parse::<f32>()
        .map_err(|_| format!("confidence must be a finite number from 0.0 to 1.0, got `{s}`"))?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!(
            "confidence must be a finite number from 0.0 to 1.0, got `{s}`"
        ));
    }
    Ok(value)
}

/// Arguments for the `capture` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle capture -m 'add login route'           # capture the worktree with intent
  heddle capture -m 'wip' --confidence 0.6      # honest confidence on a draft step

Agent automation flags (provider/model/session/policy/split) are hidden here.
Run `heddle help agent-flags`, or `heddle capture --help-agent` to list them inline.
")]
pub struct SnapshotArgs {
    /// Reveal the hidden agent-automation flags inline instead of capturing.
    /// A first-class clap flag so the whole command line (including global
    /// options in any spelling clap accepts) is parsed by clap; the dispatch
    /// arm inspects the parsed result rather than scanning raw tokens.
    /// `hide`d to keep everyday `capture --help` terse (the after-help
    /// pointer is the discovery route). It is still a registered clap arg,
    /// so `doctor docs` recognizes `heddle capture --help-agent` via the
    /// registered-but-hidden flag seam — the machine contract stays in sync
    /// without cluttering human help.
    #[arg(long, hide = true)]
    pub help_agent: bool,

    /// Natural language intent for this recoverable step.
    #[arg(short = 'm', long, visible_alias = "message")]
    pub intent: Option<String>,

    /// Confidence level (0.0-1.0).
    #[arg(long, value_parser = parse_confidence)]
    pub confidence: Option<f32>,

    /// Allow a large or deletion-heavy capture without the safety preflight.
    #[arg(short, long)]
    pub force: bool,

    /// Override HEDDLE_AGENT_PROVIDER.
    #[arg(long, hide = true)]
    pub agent_provider: Option<String>,

    /// Override HEDDLE_AGENT_MODEL.
    #[arg(long, hide = true)]
    pub agent_model: Option<String>,

    /// Override active agent session id.
    #[arg(long, hide = true)]
    pub agent_session: Option<String>,

    /// Override active agent session segment.
    #[arg(long, hide = true)]
    pub agent_segment: Option<String>,

    /// Override HEDDLE_AGENT_POLICY.
    #[arg(long, hide = true)]
    pub policy: Option<String>,

    /// Omit policy attribution.
    #[arg(long, hide = true)]
    pub no_policy: bool,

    /// Omit agent attribution.
    #[arg(long, hide = true)]
    pub no_agent: bool,

    /// Split selected paths into another thread instead of capturing the whole worktree.
    #[arg(long, hide = true)]
    pub split: bool,

    /// Target thread when using `--split`.
    #[arg(long, hide = true, requires = "split")]
    pub into: Option<String>,

    /// Repository-relative path prefix to include when using `--split`.
    #[arg(long = "path", hide = true, requires = "split", value_name = "PATH")]
    pub paths: Vec<String>,
}

/// Arguments for the Git-compatible `commit` shim.
///
/// This is the daily-driver save path: it records a recoverable Heddle
/// state, plus the matching Git checkpoint in Git-overlay repositories.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Behavior:
  Heddle's commit auto-switches on the Git index: with nothing staged it commits all worktree paths (like `git commit -a`, incl. untracked); with staged paths it commits only the index (like `git commit`). Pass `--no-all` to force index-only even when nothing is staged; pass `--all` to include unstaged/untracked paths even when the index has staged paths.

Examples:
  heddle commit -m 'add login route'        # save work; Git-overlay repos also checkpoint Git
  heddle commit -m 'wip' --confidence 0.6   # record honest confidence
  heddle commit --no-all -m 'index only'    # commit only the Git index, never sweep the worktree
  heddle commit --all -m 'save everything'  # include unstaged/untracked paths even when the Git index is staged
")]
pub struct CommitArgs {
    /// Commit/capture message. `--intent` is a deliberate alias: agents
    /// (and humans) may prefer it to record WHY the change was made, not
    /// just what changed — intent is first-class in Heddle's state model.
    #[arg(short = 'm', long = "message", visible_alias = "intent")]
    pub message: Option<String>,

    /// Confidence level for the captured Heddle state (0.0-1.0).
    #[arg(long, value_parser = parse_confidence)]
    pub confidence: Option<f32>,

    /// Include unstaged and untracked paths when the Git index already has staged changes.
    #[arg(long)]
    pub all: bool,

    /// Force an index-only commit even when nothing is staged, instead of sweeping the worktree.
    #[arg(long = "no-all", conflicts_with = "all")]
    pub no_all: bool,

    /// Allow a large or deletion-heavy capture without the safety preflight.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for the Git-compatible `switch` shim.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle switch feature/auth       # switch to an existing thread
  heddle switch hd-abc123          # move the worktree to a state
  heddle start feature/auth --path ../feature-auth  # create an isolated thread
")]
pub struct SwitchArgs {
    /// Git-style branch creation is guided to Heddle's isolated thread flow.
    #[arg(short = 'b', short_alias = 'c')]
    pub create: bool,

    /// Thread name or state id.
    pub target: String,

    /// Discard uncommitted changes when checking out a state.
    #[arg(short, long)]
    pub force: bool,

    /// Print only the target thread's checkout path on stdout.
    #[arg(long, hide_short_help = true)]
    pub print_cd_path: bool,
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
  heddle log --timeline               # show agent timeline tool-call cursor
  heddle log --reflog                 # include re-attributed history
  heddle log --path src/auth.rs       # restrict to states touching a path
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

    /// Show agent timeline tool-call navigation instead of capture history.
    #[arg(long)]
    pub timeline: bool,

    /// Timeline thread to render with `--timeline`.
    #[arg(long, default_value = "main")]
    pub thread: String,

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

/// Arguments for `heddle timeline`.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineArgs {
    #[command(subcommand)]
    pub command: TimelineCommands,
}

/// Timeline navigation action commands.
#[derive(Clone, Debug, clap::Subcommand)]
pub enum TimelineCommands {
    /// Show the current timeline cursor, counts, and recovery status.
    Status(TimelineStatusArgs),

    /// Record the start of a native tool timeline step.
    #[command(name = "record-start")]
    RecordStart(TimelineRecordStartArgs),

    /// Record the finish of a native tool timeline step.
    #[command(name = "record-finish")]
    RecordFinish(TimelineRecordFinishArgs),

    /// Fork a timeline branch from a step or native harness tool call.
    #[command(after_help = "\
Examples:
  heddle timeline fork --step tls-abc --branch tlb-experiment
  heddle timeline fork --tool-call call_123 --session ses_456 --branch tlb-alt
")]
    Fork(TimelineForkArgs),

    /// Reset the logical timeline cursor, optionally materializing checkout files.
    #[command(after_help = "\
Examples:
  heddle timeline reset --step tls-abc
  heddle timeline reset --tool-call call_123 --materialize
")]
    Reset(TimelineResetArgs),

    /// Recover a pending timeline materialization after an interrupted reset/seek.
    Recover(TimelineRecoverArgs),
}

/// Shared selector arguments for timeline action commands.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineTargetArgs {
    /// Timeline thread to target.
    #[arg(long, default_value = "main")]
    pub thread: String,

    /// Constrain the target to this branch when selecting by step/current cursor.
    #[arg(long = "from-branch", value_name = "BRANCH")]
    pub from_branch: Option<String>,

    /// Target a timeline step id.
    #[arg(long, conflicts_with_all = ["tool_call", "undo", "redo", "current"])]
    pub step: Option<String>,

    /// Target a native harness tool call id, such as an OpenCode tool call id.
    #[arg(long = "tool-call", conflicts_with_all = ["step", "undo", "redo", "current"])]
    pub tool_call: Option<String>,

    /// Native harness name for `--tool-call`.
    #[arg(long, default_value = "opencode")]
    pub harness: String,

    /// Native harness session id for `--tool-call`.
    #[arg(long)]
    pub session: Option<String>,

    /// Native harness message id for `--tool-call`.
    #[arg(long)]
    pub message: Option<String>,

    /// Target the previous step from the current cursor.
    #[arg(long, conflicts_with_all = ["step", "tool_call", "redo", "current"])]
    pub undo: bool,

    /// Target the next step from the current cursor.
    #[arg(long, conflicts_with_all = ["step", "tool_call", "undo", "current"])]
    pub redo: bool,

    /// Target the current logical cursor.
    #[arg(long, conflicts_with_all = ["step", "tool_call", "undo", "redo"])]
    pub current: bool,
}

/// Arguments for `heddle timeline fork`.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineForkArgs {
    #[command(flatten)]
    pub target: TimelineTargetArgs,

    /// New timeline branch id. Generated when omitted.
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Branch reason: explicit-fork, edit-from-rewound-cursor, retry, fan-out.
    #[arg(long, default_value = "explicit-fork")]
    pub reason: String,
}

/// Arguments for `heddle timeline reset`.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineResetArgs {
    #[command(flatten)]
    pub target: TimelineTargetArgs,

    /// Materialize checkout files to the target state after moving the cursor.
    #[arg(long)]
    pub materialize: bool,

    /// Materialization mode: fail-if-dirty or capture-current-then-seek.
    #[arg(long, default_value = "fail-if-dirty")]
    pub mode: String,
}

/// Arguments for `heddle timeline recover`.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineRecoverArgs {
    /// Timeline thread to recover.
    #[arg(long, default_value = "main")]
    pub thread: String,
}

/// Arguments for `heddle timeline status`.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineStatusArgs {
    /// Timeline thread to inspect.
    #[arg(long, default_value = "main")]
    pub thread: String,
}

/// Shared scrubbed native tool-call identity for timeline recording commands.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineRecordToolArgs {
    /// Timeline thread to record into.
    #[arg(long, default_value = "main")]
    pub thread: String,

    /// Native harness name.
    #[arg(long, default_value = "opencode")]
    pub harness: String,

    /// Native harness session id.
    #[arg(long)]
    pub session: Option<String>,

    /// Native harness message id.
    #[arg(long)]
    pub message: Option<String>,

    /// Native harness tool-call id.
    #[arg(long = "tool-call")]
    pub tool_call: String,

    /// Explicit timeline step id. When omitted, Heddle derives one from the native identity.
    #[arg(long = "step-id")]
    pub step_id: Option<String>,

    /// Explicit timeline branch id. Defaults to the current timeline branch or `tlb-main`.
    #[arg(long = "branch")]
    pub branch: Option<String>,

    /// Scrubbed human summary for the native payload.
    #[arg(long = "summary")]
    pub summary: Option<String>,

    /// Hash of the native payload, never the raw payload bytes.
    #[arg(long = "payload-hash")]
    pub payload_hash: Option<String>,
}

/// Arguments for `heddle timeline record-start`.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineRecordStartArgs {
    #[command(flatten)]
    pub tool: TimelineRecordToolArgs,

    /// Stable tool name such as `bash`, `edit`, or `read`.
    #[arg(long = "tool-name", default_value = "tool")]
    pub tool_name: String,
}

/// Arguments for `heddle timeline record-finish`.
#[derive(Clone, Debug, clap::Args)]
pub struct TimelineRecordFinishArgs {
    #[command(flatten)]
    pub tool: TimelineRecordToolArgs,

    /// Tool result status: succeeded, failed, or cancelled.
    #[arg(long, default_value = "succeeded")]
    pub status: String,
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
#[command(after_help = "\
Patch compatibility:
  --patch output targets a clean `git apply` round-trip; patch(1) support is best-effort — use `git apply` for git extended headers (type changes, mode bits, empty add/delete hunks).
")]
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

    /// Output patch in standard unified-diff format. Targets a clean `git apply` round-trip; `patch(1)` is best-effort.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,
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
  - heddle merge (FF)        (restores HEAD + the merged-into thread ref to
                              the pre-merge tip; the merged-in thread is
                              untouched.)
  - heddle switch            (restores HEAD to the pre-switch state)
  - heddle thread create/drop/rename
  - heddle thread marker create/drop
  - heddle redact apply               (with --allow-redact-undo; removes the
                                       redaction record so future materializes
                                       restore the original blob bytes. Refused
                                       when a Purge has destroyed the bytes.)
  - heddle undo --redo                re-apply the most recently undone operation

Not undoable (file a follow-up if you need one):
  - heddle push / heddle fetch        (remote-affecting; out of scope)
  - heddle redact purge apply         (destructive by design; irreversible)
  - heddle start <name> --path <dir>  (refused while the materialized worktree
                                       still exists — run `heddle thread drop
                                       <name> --delete-thread` first, then
                                       re-run `heddle undo`)
  - cross-worktree shared-backend undo (no worktree registry yet; single-
                                        worktree usage is the supported
                                        configuration for 0.3)
  - redo across CLI invocations       (use `heddle undo --redo` in the same shell)
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

    /// Re-apply operations that a prior `undo` rewound.
    #[arg(long, conflicts_with = "list")]
    pub redo: bool,

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
    /// Let Heddle choose the right checkout mode.
    Auto,
    /// Create a disk checkout with shared extents when the filesystem supports it.
    Materialized,
    /// Use a virtual filesystem checkout when the mount feature is available.
    Virtualized,
    /// Copy full files into an isolated checkout.
    Solid,
}

/// Arguments for the `thread start` and top-level `start` commands.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Examples:
  heddle start feature/auth --path ../feature-auth  # create an isolated checkout
  heddle start scratch --path ../scratch            # place the checkout explicitly
  heddle start fix-flake --task 'fix CI flake'      # attach a task description

Isolated checkouts are Heddle-managed working directories. They do not contain a .git directory; use Heddle commands inside them, and run raw Git commands from the parent Git-overlay repo when needed.

`heddle start <name> --path <dir>` is the one-step form of the advanced split flow: `heddle thread create <name>` creates the ref now, and `heddle thread promote <name> --path <dir>` materializes it later. Use the split form only when you intentionally need ref-first, checkout-later staging.

Advanced (hidden) flags:
  --agent-provider/--agent-model (agent attribution for the registered thread), --parent-thread (delegated child work), --print-cd-path (print only the checkout path for shell wrappers), --daemon/--no-daemon (virtualized-mount ownership), --shared-target (workspace-shared cargo target dir). All are accepted here; they stay out of the flag list to keep everyday help terse.
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
    #[arg(long, hide = true)]
    pub agent_provider: Option<String>,

    /// AI model name for the registered agent thread.
    #[arg(long, hide = true)]
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
    #[arg(long, hide = true, conflicts_with_all = ["agent_provider", "agent_model"])]
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
        hide = true,
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
        hide = true,
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
    #[arg(long, hide = true)]
    pub shared_target: bool,

    /// Symlink the origin checkout's top-level ignored dependency
    /// directories (`node_modules`, `.venv`, `target`, …) into this
    /// isolated checkout so it's immediately buildable — run
    /// `tsc`/`eslint`/tests without reinstalling deps from scratch.
    ///
    /// The links point back at the origin's directories and stay
    /// ignored, so the deps are never captured into heddle. Admin dirs
    /// (`.git`, `.heddle`) are excluded; only top-level ignored
    /// directories are linked. Has no effect on virtualized (mounted)
    /// threads.
    #[arg(long)]
    pub hydrate: bool,
}

/// Arguments for the `merge` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Everyday managed-thread flow:
  heddle land --thread feature/auth --no-push

Advanced/manual merge examples:
Examples:
  heddle merge feature/auth --preview         # structured blockers + recommendation
  heddle merge feature/auth -m 'merge auth'   # integrate with a commit message
  heddle merge feature/auth --with-diff       # preview with the resulting diff
  heddle merge feature/auth --no-semantic     # opt out to hunk-only merge
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

    /// Use the hunk-only merge strategy instead of the semantic merge
    /// engine. Semantic merge is the default when the `semantic` cargo
    /// feature is compiled in.
    #[arg(long = "no-semantic")]
    pub no_semantic: bool,

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

    /// Workspace mode for the ephemeral thread. Defaults to `materialized`
    /// (a real isolated checkout) so `<cmd>` runs against a proper
    /// filesystem. Pass `auto`, `virtualized`, or `solid` to use a different
    /// workspace strategy.
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

    /// Honest confidence estimate (0.0-1.0) if `ready` captures outstanding work.
    #[arg(long, value_parser = parse_confidence)]
    pub confidence: Option<f32>,
}

/// Arguments for the `sync` command.
#[derive(Clone, Debug, clap::Args)]
pub struct SyncArgs {
    /// Optional sync target. Omit for operator/thread sync.
    #[cfg(feature = "git-overlay")]
    #[command(subcommand)]
    pub command: Option<SyncCommands>,

    /// Thread to refresh (default: current thread).
    #[arg(long = "thread")]
    pub thread: Option<String>,
}

/// Arguments for the `land` command.
#[derive(Clone, Debug, clap::Args)]
pub struct LandArgs {
    /// Thread to capture, integrate, and optionally push (default: current thread).
    #[arg(long = "thread")]
    pub thread: Option<String>,

    /// Intent/message to use if land needs to capture outstanding work first.
    #[arg(short = 'm', long)]
    pub message: Option<String>,

    /// Preserve per-State Git export instead of squashing the landed thread.
    #[arg(long)]
    pub no_squash: bool,

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

    /// Discard dirty work in the source checkout while promoting.
    #[arg(long)]
    pub force: bool,
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

    /// Discard uncommitted changes in the thread checkout before dropping it.
    #[arg(short, long)]
    pub force: bool,
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

/// Arguments for the `expand` command.
#[derive(Clone, Debug, clap::Args)]
pub struct ExpandArgs {
    /// Git OID, state spec, or thread name for the squashed land.
    pub reference: String,
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

    /// Mark the path resolved even if conflict markers are still present.
    #[arg(long)]
    pub force: bool,

    /// Abort the merge.
    #[arg(long)]
    pub abort: bool,
}

/// The `(remote, thread)` pair shared by remote commands that use an
/// option-only thread selector.
#[derive(Clone, Debug, clap::Args)]
pub struct RemoteOperationArgs {
    /// Remote name, local path, URL, or hosted address.
    pub remote: Option<String>,

    /// Thread to act on.
    #[arg(short, long)]
    pub thread: Option<String>,
}

/// Arguments for the `push` command.
#[derive(Clone, Debug, clap::Args)]
pub struct PushArgs {
    /// Remote name, local path, URL, or hosted address.
    pub remote: Option<String>,

    /// Thread to push.
    #[arg(short, long, conflicts_with = "thread_arg")]
    pub thread: Option<String>,

    /// Thread to push; alias for `--thread`.
    #[arg(value_name = "THREAD")]
    pub thread_arg: Option<String>,

    /// State to push (default: HEAD).
    #[arg(short, long)]
    pub state: Option<String>,

    /// Force push.
    #[arg(short, long)]
    pub force: bool,

    /// Ad-hoc dual-push: after the primary push to the heddle remote
    /// succeeds, also push to the named Git remote (default
    /// `origin`). Use `--mirror` alone for `origin`, or `--mirror=<name>`
    /// to target a specific Git remote. The Git mirror push is best-effort:
    /// if it fails, the primary push is still reported as successful
    /// and the mirror failure surfaces as a warning.
    ///
    /// `require_equals` pairs with `default_missing_value` (clap
    /// requires both, or the next token after `--mirror` would be
    /// swallowed as the Git mirror value — silently consuming the
    /// positional primary remote).
    #[arg(
        long,
        value_name = "REMOTE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "origin",
    )]
    pub mirror: Option<String>,

    /// Push every Heddle thread, Git tag visible to this checkout, and Heddle note ref in Git-overlay mode.
    ///
    /// Without this flag, Git-overlay push sends the current branch plus
    /// refs/notes/heddle and skips Git tags.
    #[arg(long)]
    pub all_threads: bool,
}

impl PushArgs {
    pub fn thread_name(&self) -> Option<String> {
        self.thread.clone().or_else(|| self.thread_arg.clone())
    }
}

/// Arguments for the `pull` command.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Advanced (hidden) flags:
  --lazy leaves blob content absent by design and hydrates it explicitly later. Hosted/network Heddle remotes only; Git-overlay pulls reject it today — lazy hydration over the Git transport is planned for v0.3.1.
")]
pub struct PullArgs {
    #[command(flatten)]
    pub remote_op: RemoteOperationArgs,

    /// Local thread to update.
    #[arg(short, long)]
    pub local_thread: Option<String>,

    /// Leave blob content absent by design and hydrate it explicitly later.
    #[arg(long, hide = true)]
    pub lazy: bool,
}

/// Arguments for the `clone` command.
///
/// Help style budget (heddle#652): `--help` carries the signature, flags,
/// a one-screen Behavior summary, and the hidden-flag breadcrumb
/// (heddle#646). The full default-thread fallback chain and --depth
/// exposition moved to `heddle help clone` (help.rs CLONE_TOPIC); keep
/// flag docs single-line so clap renders the compact help layout.
#[derive(Clone, Debug, clap::Args)]
#[command(after_help = "\
Behavior:
  Git-overlay clones land on the remote's default branch; Heddle remotes check out `main` (pass --thread to pick another). --depth N limits history on Heddle remotes only. Never prompts. Full details: `heddle help clone`.

Advanced/planned flags: see `heddle help clone`.

Examples:
  heddle clone https://example.com/repo.git ./clone   # Git repo: lands on the remote's default branch
  heddle clone heddle://host/repo ./clone --depth 1   # shallow Heddle clone: tip plus immediate parents
")]
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

    // Hosted/network remotes only; Git-overlay clones reject it today —
    // lazy hydration over the Git transport is planned for v0.3.1. The
    // user-facing exposition lives in the after-help breadcrumb above and
    // `heddle help clone`.
    /// Leave blob content absent by design and hydrate it explicitly later.
    #[arg(long, hide = true)]
    pub lazy: bool,

    // Only `blob:none` is accepted (a synonym for --lazy on hosted
    // remotes); git-style filters such as `tree:0` or `blob:limit=…` are
    // rejected at parse time, and Git-overlay clones reject the flag at
    // runtime until v0.3.1. See the after-help breadcrumb and
    // `heddle help clone`.
    /// Partial-clone filter spec (`blob:none` only).
    #[arg(long, hide = true, value_name = "SPEC", value_parser = parse_clone_filter_spec)]
    pub filter: Option<String>,

    /// Clone a whole hosted monorepo: resolve the root spool's child tree and
    /// clone every child spool at its anchored state into its mount path.
    /// Hosted/network remotes only. (Alias: --monorepo.)
    #[arg(long, visible_alias = "monorepo")]
    pub recursive: bool,
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

    /// Attach the actor to the current thread instead of minting a new
    /// `actor/<session>` thread. Use this to record the detected agent
    /// identity without leaving a stray thread behind.
    #[arg(long, conflicts_with = "thread")]
    pub no_thread: bool,

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

    /// Local agent task assignment id to attach to this reservation.
    #[arg(long)]
    pub task_id: Option<String>,

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

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum AgentTaskStatusArg {
    Open,
    InProgress,
    Blocked,
    Complete,
    Abandoned,
}

/// Arguments for `agent task create`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentTaskCreateArgs {
    /// Optional caller-provided task id (default: generated task UUIDv7 id).
    #[arg(long)]
    pub task_id: Option<String>,

    /// Human-readable task title.
    #[arg(long)]
    pub title: String,

    /// Detailed task body.
    #[arg(long)]
    pub body: Option<String>,

    /// Thread this task targets.
    #[arg(long)]
    pub thread: String,

    /// Optional base state id this task was delegated from.
    #[arg(long)]
    pub base_state: Option<String>,

    /// Optional base root id this task was delegated from.
    #[arg(long)]
    pub base_root: Option<String>,

    /// Optional parent task id.
    #[arg(long)]
    pub parent_task_id: Option<String>,

    /// Optional coordination discussion id.
    #[arg(long)]
    pub coordination_discussion_id: Option<String>,

    /// Allow this task to continue without hosted connectivity.
    #[arg(long)]
    pub allow_offline: bool,

    /// Principal or agent that delegated this task.
    #[arg(long)]
    pub delegated_by: Option<String>,
}

/// Arguments for `agent task list`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentTaskListArgs {
    /// Filter by target thread.
    #[arg(long)]
    pub thread: Option<String>,

    /// Filter by task status.
    #[arg(long)]
    pub status: Option<AgentTaskStatusArg>,
}

/// Arguments for `agent task show`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentTaskShowArgs {
    /// Task id to show.
    pub task_id: String,
}

/// Arguments for `agent task update`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentTaskUpdateArgs {
    /// Task id to update.
    pub task_id: String,

    /// Replace the task title.
    #[arg(long)]
    pub title: Option<String>,

    /// Replace the task body.
    #[arg(long)]
    pub body: Option<String>,

    /// Replace the task status.
    #[arg(long)]
    pub status: Option<AgentTaskStatusArg>,

    /// Replace the target thread.
    #[arg(long)]
    pub thread: Option<String>,

    /// Replace the base state id.
    #[arg(long)]
    pub base_state: Option<String>,

    /// Replace the base root id.
    #[arg(long)]
    pub base_root: Option<String>,

    /// Replace the parent task id.
    #[arg(long)]
    pub parent_task_id: Option<String>,

    /// Replace the coordination discussion id.
    #[arg(long)]
    pub coordination_discussion_id: Option<String>,

    /// Allow this task to continue without hosted connectivity.
    #[arg(long, conflicts_with = "no_allow_offline")]
    pub allow_offline: bool,

    /// Disallow offline continuation for this task.
    #[arg(long, conflicts_with = "allow_offline")]
    pub no_allow_offline: bool,

    /// Replace the delegating principal or agent label.
    #[arg(long)]
    pub delegated_by: Option<String>,
}

/// Arguments shared by `agent fanout plan` and `agent fanout start`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentFanoutPlanArgs {
    /// Parent coordination task title.
    #[arg(long)]
    pub title: String,

    /// Lane spec: `<thread>=<path>:<title>`. Repeat once per child lane.
    #[arg(long, value_name = "THREAD=PATH:TITLE")]
    pub lane: Vec<String>,

    /// Optional collaboration discussion id to store on task assignments.
    #[arg(long)]
    pub coordination_discussion_id: Option<String>,
}

/// Arguments for `agent fanout start`.
#[derive(Clone, Debug, clap::Args)]
pub struct AgentFanoutStartArgs {
    /// Parent coordination task title.
    #[arg(long)]
    pub title: String,

    /// Lane spec: `<thread>=<path>:<title>`. Repeat once per child lane.
    #[arg(long, value_name = "THREAD=PATH:TITLE")]
    pub lane: Vec<String>,

    /// Optional collaboration discussion id to store on task assignments.
    #[arg(long)]
    pub coordination_discussion_id: Option<String>,
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
    #[arg(long, value_parser = parse_confidence)]
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

    /// Honest confidence estimate (0.0-1.0) if `agent ready` captures outstanding work.
    #[arg(long, value_parser = parse_confidence)]
    pub confidence: Option<f32>,
}

/// Arguments for the `watch` command.
///
/// Streams live oplog activity (snapshots, merges, thread create/update,
/// markers, etc.) as it happens. Default behavior tails forever and exits
/// on Ctrl-C. `--since 5m` replays the last N before tailing live;
/// `--filter` restricts output to the named kinds; `--output json` emits one
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
    /// collapse,thread_marker_create,thread_marker_delete`).
    #[arg(long, value_name = "KINDS")]
    pub filter: Option<String>,

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
mod capture_message_alias_tests {
    use clap::Parser;

    use crate::cli::{Cli, Commands, SnapshotArgs};

    fn parse_capture(extra: &[&str]) -> Result<SnapshotArgs, clap::Error> {
        let mut argv: Vec<&str> = vec!["heddle", "capture"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv)?;
        match cli.command {
            Commands::Capture(args) => Ok(args),
            _ => panic!("expected Commands::Capture"),
        }
    }

    #[test]
    fn capture_accepts_message_alias() {
        let args = parse_capture(&["--message", "my change"]).expect("--message should parse");
        assert_eq!(args.intent.as_deref(), Some("my change"));
    }

    #[test]
    fn capture_accepts_intent_long_form() {
        let args = parse_capture(&["--intent", "my change"]).expect("--intent should parse");
        assert_eq!(args.intent.as_deref(), Some("my change"));
    }

    #[test]
    fn capture_accepts_short_m() {
        let args = parse_capture(&["-m", "my change"]).expect("-m should parse");
        assert_eq!(args.intent.as_deref(), Some("my change"));
    }

    #[test]
    fn capture_rejects_non_finite_or_out_of_range_confidence() {
        for value in ["NaN", "inf", "-0.1", "1.7"] {
            let confidence_arg = format!("--confidence={value}");
            let err = parse_capture(&["-m", "bad confidence", &confidence_arg])
                .expect_err("invalid confidence should fail to parse");
            assert!(
                err.to_string()
                    .contains("confidence must be a finite number from 0.0 to 1.0"),
                "unexpected parse error for {value}: {err}"
            );
        }
    }
}

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
