# Command Context And Repo Shape Policy

Status: Wave 1 planning note. This document sketches the pattern Wave 2 typed
command pipelines should implement for `status`, `capture`, and `merge`. It is
not shipped behavior and does not imply a CLI behavior change until the command
modules adopt the shapes below.

This note covers two linked modules:

- borrowed command contexts: small command-local fact bundles passed from gather
  to compute, render, and verify
- repo shape policy: a named decision interface for native Heddle, Git overlay,
  plain Git, materialized checkout, and unsupported checkout shapes

The goal is locality: command policy should live in one place per command, and
expensive repository facts should be gathered once per invocation.

## Existing Precedent

The current codebase already has the pattern in smaller forms:

- `crates/cli/src/cli/commands/diff/diff_compute.rs` has
  `DiffGatherContext<'repo>` with a borrowed `&Repository` plus owned resolved
  states and trees.
- `crates/repo/src/status_tracked_refresh.rs` and
  `crates/repo/src/status_untracked_scan.rs` use private borrowed scan contexts
  to thread a repo, index, monitor session, mutable status, and stats through
  recursive worktree scans.
- `crates/cli/src/cli/commands/merge/mod.rs` has `MergeAttemptPlan`, which
  decides semantic strategy once and feeds preview, apply, and diff paths.
- `crates/cli/src/cli/commands/git_overlay_health/mod.rs` has
  `build_repository_verification_state_with_worktree_status`, which preserves a
  single `git_overlay_worktree_status()` result across health and verification.

Wave 2 should extend these local modules rather than adding one universal
`CommandContext`.

## Context Rules

Command contexts should:

- stay command-specific
- borrow the open `Repository` and config values where possible
- own expensive facts that are returned by value, such as `State`, `Tree`,
  `WorktreeStatus`, `Thread`, and rendered plan objects
- preserve `Result<Option<WorktreeStatus>>` for Git-overlay status until every
  consumer that needs the exact classification has read it
- separate gather facts from compute decisions and render output
- avoid hidden mutation in gather, except explicit read-time reconciliation that
  already happens in `Repository::open`

Command contexts should not:

- become a bag of optional fields shared by unrelated commands
- re-open the repository from render or verify
- call `repo.head()`, `repo.current_state()`, status scans, remote tracking, or
  thread summary walks again after gather unless the command intentionally needs
  fresh post-mutation state
- cache a pre-mutation fact for post-mutation verification when the mutation
  changes the meaning of that fact

## Status Context

`status` currently gathers most facts inside `build_status_output`. Wave 2
should split that function into gather, compute, render, and verify without
changing its JSON or text contracts.

Target shape:

```rust
struct StatusModeFacts {
    render_json: bool,
    compact_json: bool,
    short_text: bool,
    short_path: bool,
    needs_full_walk: bool,
    needs_remote_tracking: bool,
}

struct StatusGatherContext<'repo> {
    repo: &'repo Repository,
    mode: StatusModeFacts,
    current_state: Option<State>,
    operation: Option<RepositoryOperationStatus>,
    raw_remote_tracking: Option<GitRemoteTrackingStatus>,
    import_hint: Option<GitOverlayImportHint>,
    git_worktree_status: repo::Result<Option<WorktreeStatus>>,
    git_overlay_health: GitOverlayHealth,
    trust: RepositoryVerificationState,
    git_index: Option<GitIndexPlan>,
    status_options: repo::WorktreeStatusOptions,
    identity_notice: Option<String>,
}

struct StatusComputedFacts {
    changes: ChangesInfo,
    worktree_profile: Option<WorktreeCompareProfile>,
    remote_tracking: Option<GitRemoteTrackingStatus>,
    current_lane: Option<String>,
    thread_summary: Option<ThreadSummary>,
    parallel_threads: Vec<ThreadSummary>,
    materialized_threads: Vec<MaterializedThreadInfo>,
    repository_presentation: RepositoryPresentation,
    advice: StatusAdvice,
}
```

The exact concrete type names for `ThreadSummary`, `RepositoryPresentation`, and
`StatusAdvice` can follow existing private module names. The important
interface is the lifetime and ownership pattern.

Gather once:

- output mode: `should_output_json`, `output_is_compact`, `short_text`,
  `short_path`, `needs_full_walk`, `needs_remote_tracking`
- repository open and current state
- operation status
- remote tracking only when `needs_full_walk || short_text`
- import hint only outside the short path
- `git_overlay_worktree_status()` exactly once, preserving the raw result for
  health, verification, and change classification
- Git-overlay health from the same status result
- verification state from the same health and status result
- worktree status or cached comparison for native Heddle only once
- full thread summaries only when JSON full output or verbose text will render
  cross-thread relationships
- materialized thread assessment once per output build

Pipeline example:

```text
gather:
  open repo
  derive StatusModeFacts
  read current_state, operation, optional remote tracking, optional import hint
  read git_overlay_worktree_status once
  build health and trust from that same status result
  read git index and identity notice

compute:
  derive ChangesInfo from the precomputed Git status or one native cached scan
  select short-path or full thread summary walk
  build repository presentation after target/parent thread are known
  compute advice, blockers, recommended action, and coordination status

render:
  render StatusOutput only from StatusGatherContext + StatusComputedFacts
  validate next actions with the gathered repository capability

verify:
  JSON schema and command tests assert output compatibility
  focused tests assert Git-overlay status is reused across health/trust/changes
```

Freshness rule: `status` is read-only, so the precomputed status and trust can
flow through render. No post-render repository read should change output.

## Snapshot Context

`capture` already computes Git-overlay worktree status once for the large
capture preflight and `create_snapshot`. Wave 2 should broaden that into a
command-local context and remove the remaining repeated "is there anything to
capture" scans.

Target shape:

```rust
struct SnapshotInputs<'cmd> {
    intent: &'cmd str,
    confidence: Option<f32>,
    force: bool,
    agent: &'cmd SnapshotAgentOverrides,
}

struct SnapshotGatherContext<'repo, 'cmd> {
    repo: &'repo Repository,
    user_config: &'cmd UserConfig,
    inputs: SnapshotInputs<'cmd>,
    current_state: Option<State>,
    base_tree: Option<Tree>,
    merge_resolution_complete: bool,
    pre_capture_worktree_status: repo::Result<Option<WorktreeStatus>>,
    native_worktree_status: Option<WorktreeStatus>,
    hook_manager: HookManager,
    hook_context: HookContext,
}

struct SnapshotComputedFacts {
    has_capture_work: bool,
    attribution: Attribution,
    large_capture_preflight: Option<RecoveryAdvice>,
    mutation_preflight: Option<RecoveryAdvice>,
}

struct SnapshotMutationResult {
    output: SnapshotOutput,
    profile: SnapshotCommandProfile,
    state: State,
    tree: Tree,
}
```

`HookManager` and `HookContext` are owned structs derived from the repository.
They should be created once and passed through the pre-hook, snapshot, and
post-hook phases.

Gather once:

- validated non-empty capture intent
- user config
- current state and base tree used for "nothing to capture" checks
- merge-state resolution completeness
- Git-overlay worktree status result used by large-capture preflight and
  mutation preflight
- native cached worktree status when the repository is not Git-overlay or when
  current state is already Heddle-backed
- hook manager and hook context
- attribution inputs from config/env, then the final `Attribution` before the
  mutation

Pipeline example:

```text
gather:
  resolve command start path through repo shape policy
  open repo and load user config
  read current_state and base_tree once
  read merge-state completeness
  read pre_capture_worktree_status once
  read native status once only when Git-overlay status is not the right source
  build hook objects once

compute:
  decide has_capture_work from gathered status/tree facts
  run large-capture and mutation preflights from gathered status facts
  build Attribution from user config and agent overrides

render:
  render SnapshotOutput returned by the mutation
  do not re-open repo for principal, agent, or action metadata

verify:
  build post-capture verification state fresh after the mutation
  update Git intent-to-add and tips from the new current state, not from
  pre_capture_worktree_status
```

Freshness rule: post-capture verification is intentionally fresh. The snapshot
mutation advances Heddle state and may change Git-overlay health, so Wave 2 must
not reuse pre-capture trust as the output verification state.

## Merge Context

`merge` already has a planning seam through `MergeAttemptPlan` and `MergePlan`.
Wave 2 should lift the surrounding repeated repository, thread, graph, and path
facts into a borrowed context without changing preview/apply behavior.

Target shape:

```rust
struct MergeInputs<'cmd> {
    track_name: &'cmd str,
    message: Option<&'cmd str>,
    no_commit: bool,
    preview: bool,
    with_diff: bool,
    no_semantic: bool,
    git_commit: bool,
}

struct MergeGatherContext<'repo, 'cmd> {
    repo: &'repo Repository,
    inputs: MergeInputs<'cmd>,
    attempt: MergeAttemptPlan,
    registry: AgentRegistry,
    thread_manager: ThreadManager,
    thread: Option<Thread>,
    thread_entry: Option<AgentEntry>,
    merge_target_id: ChangeId,
    current_change_id: ChangeId,
    current_state: State,
    current_thread_label: String,
    graph: CommitGraphIndex<'repo>,
}

struct MergeComputedPlan {
    preview_report: Option<ThreadPreviewReport>,
    source_uncaptured_work: Option<SourceThreadUncapturedWork>,
    merge_plan: MergePlan,
    current_label: String,
    incoming_label: String,
}

struct MergeBranchFacts {
    changed_paths: Vec<String>,
    renames: Vec<RenameEntry>,
    directory_renames: Vec<RenameEntry>,
    diff: Option<DiffOutput>,
    git_commit_preflight_blockers: Vec<String>,
}
```

Gather once:

- active target checkout path through `Repository::active_worktree_path()`
  before hooks and mutation
- merge inputs and `MergeAttemptPlan::decide(no_semantic)` once
- thread manager and registry from `repo.heddle_dir()`
- source thread record with freshness refreshed once
- agent registry entry for the source thread once
- merge-in-progress state before any worktree or ref mutation
- merge target id
- current Heddle state, current thread label, and current change id
- one commit graph for preview and merge planning
- preview report from the same strategy and actual current destination
- source-thread uncaptured work check
- `MergePlan` from the same graph, current state, target state, and labels

Pipeline example:

```text
gather:
  open cwd repo, resolve active_worktree_path, re-open target repo if needed
  build hook context once
  decide MergeAttemptPlan once
  read thread, registry entry, merge target, current state, current lane
  create CommitGraphIndex once

compute:
  build preview report with attempt.strategy()
  build MergePlan with attempt.strategy()
  branch on relation: already up to date, fast-forward, conflicted,
  no-commit, clean committed merge
  compute branch-specific changed paths, renames, diffs, and git preflights
  from the plan and current/target ids

render:
  build MergeOutput from MergeOutputInput and branch facts
  scope recommendations to the CLI repo path once

verify:
  preview verifies worktree cleanliness and trust before reporting a runnable
  action
  apply verifies worktree cleanliness, merge-in-progress state, Git commit
  preconditions, and source-thread uncaptured work before mutation
  post-mutation Git commit failures become structured blockers without
  recomputing the merge plan
```

Freshness rule: preview and apply must share `MergeAttemptPlan`; any future
test for this module should keep the existing invariant that there is one
strategy decision and no bare `merge_strategy_for` calls in branch code.

## Repo Shape Policy

Current shipped behavior exposes `repo::RepositoryCapability::{NativeHeddle,
GitOverlay}` after a `Repository` is open. Plain Git is detected before open by
plain-Git verification probes, and materialized or virtualized checkout shape
is currently inferred through repository open behavior and thread metadata.

Wave 2 should add a small CLI policy module, not a giant environment object.
Suggested home:

```text
crates/cli/src/cli/repo_shape_policy.rs
```

Target API:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CheckoutShape {
    NativeHeddle,
    GitOverlay,
    PlainGitReadOnly,
    MaterializedThreadCheckout,
    VirtualizedThreadMount,
    BareStore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostedShape {
    LocalOnly,
    HostedConfigured,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HydrationShape {
    CompleteLocalObjects,
    LazyHydrationAvailable,
    LazyHydrationBoundary,
}

pub(crate) struct RepoShapeFacts<'repo> {
    pub checkout: CheckoutShape,
    pub hosted: HostedShape,
    pub hydration: HydrationShape,
    pub root: PathBuf,
    pub repo: Option<&'repo Repository>,
}

pub(crate) struct RepoShapePolicy<'repo> {
    facts: RepoShapeFacts<'repo>,
}

impl<'repo> RepoShapePolicy<'repo> {
    pub(crate) fn for_start_path(start: &Path) -> Result<RepoShapeProbe>;
    pub(crate) fn for_open_repo(repo: &'repo Repository) -> Self;

    pub(crate) fn status_plan(&self) -> StatusShapePlan;
    pub(crate) fn capture_plan(&self) -> MutationShapePlan;
    pub(crate) fn merge_target_root(&self) -> Result<PathBuf>;
    pub(crate) fn next_action_capability(&self) -> Option<repo::RepositoryCapability>;
}
```

`RepoShapeProbe` may need to own either an opened `Repository` or a plain-Git
probe. Do not force `PlainGitReadOnly` through `Repository::open`; that would
bootstrap sidecar state and change current status behavior.

Suggested command-facing plans:

```rust
pub(crate) enum StatusShapePlan {
    HeddleRepo,
    PlainGit { probe: PlainGitVerificationProbe },
    Unsupported { advice: RecoveryAdvice },
}

pub(crate) enum MutationShapePlan {
    Allowed,
    PlainGitRefusal { advice: RecoveryAdvice },
    GitOverlayPreflightRequired,
    Unsupported { advice: RecoveryAdvice },
}
```

Policy mapping for Wave 2:

| Shape | Detection source | Status | Capture | Merge |
|---|---|---|---|---|
| `NativeHeddle` | open repo capability is `NativeHeddle` and no worktree pointer shape applies | full Heddle status | allowed | allowed |
| `GitOverlay` | open repo capability is `GitOverlay` | full status with Git-overlay health | allowed after Git-overlay mutation preflights | allowed after verification and Git commit preflights |
| `PlainGitReadOnly` | plain-Git verification probe succeeds before Heddle open | render setup/adopt guidance without side effects | refuse with plain-Git mutation advice | refuse unless future command explicitly supports read-only preview |
| `MaterializedThreadCheckout` | `.heddle/objectstore` pointer or thread metadata says this checkout has a dedicated root | status/capture operate on this checkout | capture stays CWD-based | merge/goto/rebase resolve through `active_worktree_path()` |
| `VirtualizedThreadMount` | `Repository::open` would hit the metadataless managed mount refusal | render explicit unsupported checkout advice | refuse | refuse |
| `BareStore` | exact open of a store-only repo, not current default discovery | read-only inspection only unless a command opts in | refuse | refuse |

Hosted and hydration should stay orthogonal policy axes. A Git-overlay repo can
also be hosted-configured. A hosted or Git-overlay clone can also have lazy blob
hydration. Commands should ask for the axis they need rather than matching a
large cross-product enum.

## Verification Expectations

Docs-only Wave 1 needs no cargo run. Wave 2 implementation should add tests at
the interface where behavior changes:

- status: JSON compatibility tests, plain-Git setup text tests, Git-overlay
  health tests, and a focused reuse test if a local status seam can count scans
- capture: no-work refusal, plain-Git mutation refusal, Git-overlay dirty/clean
  preflights, post-capture verification freshness, and hook order
- merge: preview/apply parity, active worktree target selection, dirty worktree
  refusal, Git commit preflight blockers, and no duplicate strategy decision
- repo shape policy: pure unit tests for enum mapping plus CLI integration tests
  for recovery text in plain Git, Git overlay, materialized checkout, and
  virtualized mount cases

The behavior contract is simple: moving facts into a borrowed context must
first be behavior-identical, then performance claims can be made only with a
fixture, benchmark, or targeted smoke test.
