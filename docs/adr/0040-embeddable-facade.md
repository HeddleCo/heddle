# Embeddable facade architecture

Heddle becomes an embeddable library whose operations return typed data and never
touch process control, I/O side-channels, or ambient global state. The CLI is
demoted to a thin `clap → facade → render` shell in front of that library. This
ADR locks the foundational decisions every downstream extraction wave of the
0.5.0 hardening campaign (`reviews/heddle-hardening-plan.md`) consumes, so each
"extract X into the facade" wave can proceed without re-litigating the boundary.

The decisions are concrete on purpose: a downstream agent picking up
"extract X-ops into the facade" must be able to read off the target crate, the
result-type location, the `ExecutionContext` it receives, and the render
boundary — without asking the maintainer.

## Context — what the code looks like today (2026-06-21, main `20698831`)

- `crates/cli` (package `heddle-cli`, `crates/cli/Cargo.toml:2`) is ~93k LOC of
  commands plus a ~7.3k-LOC git bridge under `crates/cli/src/bridge/`
  (`git_core.rs` alone is 4,152 LOC). ~17–19k LOC of business logic is trapped
  in this binary crate.
- No logic crate depends back on `heddle-cli` — the dependency graph is acyclic
  with `cli` at the apex (verified across every non-cli `Cargo.toml`). Extraction
  is therefore structurally unblocked.
- Every command takes `&Cli`, the clap struct (`crates/cli/src/cli/cli_args/cli_base.rs:46`)
  carrying `--output` (`:57`), `--no-color` (`:61`), `--repo`/`-C` (`:64`),
  `--verbose` (`:68`), `--quiet` (`:72`), and `--op-id` (`:81`). Commands resolve
  the repo ambiently through `Cli::open_repo()` (`cli_base.rs:90`), which reads
  the cwd when `--repo` is absent.
- The dispatch is one big `match &cli.command` in `crates/cli/src/main.rs:286`;
  every arm calls a `cmd_*(&cli, …)` handler.
- Output formatting is fused into command bodies — compute and serialize in the
  same function. `cmd_status` (`crates/cli/src/cli/commands/status.rs:330`,
  `async fn … -> Result<()>`) computes status then both `write_command_json(…)`
  (`status.rs:432`) and `println!("{}", style::bold("Heddle status"))`
  (`status.rs:443`) live in the same body. `cmd_merge`
  (`crates/cli/src/cli/commands/merge/mod.rs:188`, `fn … -> Result<()>`) does the
  same at `merge/mod.rs:2523` (json) and `:2542` (text).
- There are 18 `process::exit` sites (the plan's "~15" undercounted). Production
  CLI sites that should become `Result`: `main.rs:255/264/277/860`,
  `operation_id.rs:147/193`, `operator_core.rs:248`, `thread_approval.rs:331`,
  `snapshot.rs:216`, `try_cmd.rs:278` (child-exit passthrough), `merge/mod.rs:263`.
  The legitimate `main()` exits are `main.rs:169/172/217` (parse/config bootstrap,
  before any command runs). `crates/ingest/src/bin/main.rs:344/548/555` and
  `crates/devtools/src/fuse_dispatch_bench.rs:152` are separate bin tools, out of
  facade scope.
- Logic crates leak I/O. Production warnings that must move to a structured
  channel: `crates/refs/src/refs/refs_storage.rs:33` (lock-release failure in a
  `Drop`) and `crates/oplog/src/oplog/packed_oplog.rs:1596` (corruption-recovery
  warning). `ingest` (82) and `semantic` (42) print counts are dominated by their
  `src/bin/` tools and test/bench modules; the genuine library-path prints route
  the same way.
- Ambient state via `OnceLock`/static singletons blocks running two operations
  (or two repos) in one process: `USER_CONFIG` (`crates/cli/src/cli/mod.rs:49`),
  color `COLOR_STATE` (`crates/cli/src/cli/style.rs:44`, set by
  `init_from_cli` at `:54`), lazy-hydrator `REGISTRY`
  (`crates/repo/src/lazy_hydrator.rs:196`), fault-inject `FAULT_POINTS`
  (`crates/objects/src/fault_inject.rs:43`), and the semantic AST `CACHE`
  (`crates/semantic/src/cache.rs:57`).
- The shared error type `HeddleError` already lives below the CLI, in
  `crates/objects/src/error.rs:8`. The exit-code taxonomy `HeddleExitCode`
  (`crates/cli/src/exit.rs:19`, `from_error` at `:86`) is cli-only and maps the
  error chain — keyed on a stable `kind` discriminator (`exit.rs:59`), not the
  user-visible message.
- In flight: branch `chore/lib-ify-cli-infra` (not yet merged) moves `logging`
  and `OutputMode` into `crates/cli-shared`. On main today `OutputMode`
  (`cli_args/cli_base.rs:104`) and the `logging` module
  (`crates/cli/src/logging/mod.rs`) still live in `cli`; `cli-shared` currently
  owns `UserConfig`, `ClientConfig`, and remote-target types. **This ADR assumes
  `cli-shared` owns `logging` + `OutputMode` once that branch lands** and composes
  with it.

## Decision

The three principles enforced everywhere below the `cli` line, from the plan:
**no process control** (return `Result`, never `process::exit`/`panic!` in prod),
**no I/O side-channels** (return data or write to an injected sink, never
`println!`/`eprintln!`), **no ambient state** (config/registries/caches live in a
passed-in context, not statics). The eight locked decisions follow.

### 1. Facade crate boundary — a new `heddle-core` crate

Create a new crate `crates/core`, package name **`heddle-core`**. It is the
operation API and sits one layer below `cli` and above the domain crates.

```
crates/cli (heddle-cli)        clap parse · render (text/json) · style/color ·
                               exit-code map · progress/stdout-stderr glue
        │ depends on
crates/core (heddle-core)      ExecutionContext · operation API · typed *Report
                               structs · re-exports HeddleError + observability
        │ depends on
repo · objects · merge · semantic · refs · oplog · ingest · format · wire ·
crypto · cli-shared            domain logic (Result-only, sink-reporting)
```

- **Dependencies it MAY take:** the domain crates it orchestrates (`repo`,
  `objects`, `merge`, `semantic`, `refs`, `oplog`, `ingest`, `format`, `wire`,
  `crypto`), plus `cli-shared` (for `UserConfig`/remote-target types), plus
  `serde`, `thiserror`/`anyhow`. The dependency edges stay acyclic: domain crates
  never depend on `heddle-core`.
- **Dependencies it MUST NOT take:** `heddle-cli`, `clap`, `anstyle`/`anstream`
  or any terminal/TTY/`indicatif`-style render crate. A CI grep-gate enforces
  this (`heddle-core` and domain crates may not list those deps) so the facade
  stays render-free and embeddable from a server, a daemon, or a test harness.
- **What it re-exports** (so embedders need one crate): `ExecutionContext` and its
  builder, every operation fn/module, every typed `*Report`/`*Result`/`*Output`
  struct, `HeddleError` (re-exported from `objects`), and the observability traits
  `ProgressSink`/`WarningSink` with `ProgressEvent`/`Warning` (see decision 5).
- **What it does NOT contain:** renderers, `OutputMode`, color/style state, the
  exit-code taxonomy. Those are render/process concerns owned by `cli`.

**Why not extend `cli-shared` or `repo`?** `cli-shared` is scoped to shared
config/remote *value types* (its deps are `objects`, `wire`, `repo`); making it
the operation API would balloon its dependency surface and conflate "config the
CLI and client share" with "the orchestration layer." `repo` is one domain crate
among many — the facade orchestrates across `merge`, `semantic`, `ingest`, and the
bridge, so housing it in `repo` would force `repo` to depend on all of them and
risk cycles (`semantic → repo`? `merge → repo`?). A dedicated apex-below-cli crate
is the only home that keeps the graph acyclic and the boundary legible.

### 2. `ExecutionContext` — the struct that replaces `&Cli` threading

`ExecutionContext` lives in `heddle-core`. It carries the *semantic* execution
state a command needs — and deliberately omits everything that is a render or
process concern.

```rust
// crates/core/src/context.rs
pub struct ExecutionContext {
    /// Already-opened repository handle, or None for repo-creating ops
    /// (init / clone / adopt). The facade NEVER re-derives the repo from cwd;
    /// the caller resolves --repo / cwd and hands a handle in. Repository's
    /// internal state is Arc-backed, so holding it here is cheap.
    repo: Option<Repository>,
    /// Replaces the USER_CONFIG OnceLock (cli/mod.rs:49). Loaded once at
    /// construction; lives here, not in a static.
    config: UserConfig,                 // from cli-shared
    /// Semantic detail level (replaces -v/-q for "how much to compute/report").
    /// NOT a log-level: the tracing subscriber stays a cli concern.
    verbosity: Verbosity,
    /// Replaces logic-crate eprintln! progress. No-op by default (decision 5).
    progress: Arc<dyn ProgressSink>,
    /// Replaces logic-crate eprintln! warnings (refs Drop, oplog corruption…).
    warnings: Arc<dyn WarningSink>,
    /// Idempotency key from --op-id / HEDDLE_OPERATION_ID. Optional.
    op_id: Option<OperationId>,
    /// Replaces the FAULT_POINTS OnceLock (objects/fault_inject.rs:43).
    /// Empty for embedders; env-seeded only by the CLI constructor.
    faults: FaultConfig,
    /// Replaces the semantic CACHE OnceLock (semantic/cache.rs:57). Held here
    /// so two operations in one process can share or isolate it by choice.
    semantic_cache: Arc<SemanticCache>,
}

impl ExecutionContext {
    /// Borrow the repo or fail with a typed "not in a repository" error.
    pub fn require_repo(&self) -> Result<&Repository, HeddleError> { … }
    pub fn progress(&self) -> &dyn ProgressSink { &*self.progress }
    pub fn warnings(&self) -> &dyn WarningSink { &*self.warnings }
    pub fn config(&self) -> &UserConfig { &self.config }
}
```

- **Explicitly NOT in the context:** `OutputMode`, color/style, exit codes. The
  hardening plan's F1 sketch put `output` in the context; this ADR overrides that
  — output mode is a *render selection*, owned by `cli`. An embedder receives
  typed structs and chooses its own representation, so it never needs `OutputMode`.
  This sharpens the facade boundary.
- **CLI construction:**
  `ExecutionContext::from_cli(cli: &Cli) -> Result<Self, HeddleError>` performs
  the `--repo`/cwd resolution currently in `Cli::open_repo()` (`cli_base.rs:90`),
  loads `UserConfig`, seeds `faults` from `HEDDLE_FAULT_INJECT`, and installs a
  *CLI* progress sink (drives a stderr bar) and warning sink (formats to stderr).
- **Embedder construction:** a builder —
  `ExecutionContext::builder().repo(handle).config(cfg).progress(Arc::new(NoopProgress)).warnings(Arc::new(CollectingWarnings::default())).build()`.
  The embedder supplies its own opened `Repository`, a no-op or custom progress
  sink, and a warning sink that captures into a `Vec` instead of printing.
- **How a command gets the repo:** `ctx.require_repo()?` (or the op takes a path
  param for init/clone/adopt and returns the new `Repository`).

#### Before / after — `status`

```rust
// BEFORE — crates/cli/src/cli/commands/status.rs:330
pub async fn cmd_status(cli: &Cli, short: bool, watch: bool,
                        watch_iterations: Option<usize>,
                        watch_interval_ms: Option<u64>) -> Result<()> {
    let repo = cli.open_repo()?;                 // ambient repo resolution
    /* …compute status… */
    if should_output_json(cli) { write_command_json(/*…*/)?; }   // render fused in
    else { println!("{}", style::bold("Heddle status")); /*…*/ } // here
    Ok(())
}
```

```rust
// AFTER — logic in heddle-core, render in cli
// crates/core/src/status.rs
pub fn status(ctx: &ExecutionContext, opts: StatusOptions)
    -> Result<StatusReport, HeddleError>
{
    let repo = ctx.require_repo()?;
    /* …compute only… */
    Ok(StatusReport { branch, ahead, behind, entries, /*…*/ })   // returns DATA
}

// crates/cli/src/cli/commands/status.rs  (now thin)
pub async fn cmd_status(cli: &Cli, short: bool, watch: bool, /*…*/) -> Result<()> {
    let ctx = ExecutionContext::from_cli(cli)?;
    // `watch` is a CLI loop concern — the CLI re-calls the single-shot facade op.
    let report = heddle_core::status(&ctx, StatusOptions { short })?;
    match cli.output_mode() {
        OutputMode::Text                  => render::status_text(&report, cli.style()),
        OutputMode::Json | JsonCompact    => render::status_json(&report, cli.output_mode()),
    }
    Ok(())
}
```

The `watch` loop, color, and json-vs-text choice never cross into `heddle-core`.
`merge` follows the identical shape: `heddle_core::merge(&ctx, MergeOptions{…}) ->
Result<MergeReport, HeddleError>`, and the `process::exit` at `merge/mod.rs:263`
becomes a `return Err(…)` mapped by `main()` (decision 6).

### 3. Operation API surface

Inputs are plain Rust option structs (defined in `heddle-core`, NOT clap structs);
the CLI's clap arg structs convert into them. Outputs are typed `*Report` structs
(decision 4). Every operation returns `Result<T, HeddleError>`. "Logic home" is
where the *compute* lands after extraction; `heddle-core` is the assembly/orchestration
point and the home for compute that is genuinely cross-domain.

| Operation | Facade signature (in `heddle-core`) | Result type | Logic home (WU) |
|---|---|---|---|
| init | `init(&Ctx, InitOptions) -> Result<InitReport>` | `InitReport` | `repo` |
| adopt / import | `adopt(&Ctx, AdoptOptions) -> Result<AdoptReport>` | `AdoptReport` | `repo` + bridge (X-bridge) |
| status | `status(&Ctx, StatusOptions) -> Result<StatusReport>` | `StatusReport` | `core::status` (X-status) |
| capture / commit | `capture(&Ctx, CaptureOptions) -> Result<CaptureReport>` | `CaptureReport` | `repo` / `oplog` |
| diff | `diff(&Ctx, DiffOptions) -> Result<DiffReport>` | `DiffReport` (borrowed `LineDiff<'_>` later, Z3) | `core::diff` (X-diff) |
| merge | `merge(&Ctx, MergeOptions) -> Result<MergeReport>` | `MergeReport` | `merge` (X-ops) |
| resolve | `resolve(&Ctx, ResolveOptions) -> Result<ResolveReport>` | `ResolveReport` | `merge` (X-ops) |
| rebase | `rebase(&Ctx, RebaseOptions) -> Result<RebaseReport>` | `RebaseReport` | `repo`/`merge` (X-ops) |
| cherry-pick | `cherry_pick(&Ctx, CherryPickOptions) -> Result<CherryPickReport>` | `CherryPickReport` | `repo`/`merge` (X-ops) |
| undo / redo | `undo(&Ctx, UndoOptions) -> Result<UndoReport>` · `redo(…)` | `UndoReport` | `repo`/`oplog` (X-ops) |
| log / history | `log(&Ctx, LogOptions) -> Result<LogPage>` | `LogPage` | `oplog` |
| query | `query(&Ctx, QueryOptions) -> Result<QueryResult>` | `QueryResult` | `oplog` |
| clone | `clone(&Ctx, CloneOptions) -> Result<CloneReport>` | `CloneReport` | `repo`/`wire`/`client` |
| fetch | `fetch(&Ctx, FetchOptions) -> Result<FetchReport>` | `FetchReport` | `wire`/`client` |
| push | `push(&Ctx, PushOptions) -> Result<PushReport>` | `PushReport` | `wire`/`client` |
| pull | `pull(&Ctx, PullOptions) -> Result<PullReport>` | `PullReport` | `wire`/`client` |
| fsck | `fsck(&Ctx, FsckOptions) -> Result<FsckReport>` | `FsckReport` | `repo`/`objects` |
| gc / maintenance | `gc(&Ctx, GcOptions) -> Result<GcReport>` | `GcReport` | `objects`/`repo` |
| thread ops | `thread::start/switch/land/ready/abort/continue(&Ctx, …)` | `Thread*Report` | `repo` |
| discuss / context | `discuss(&Ctx, DiscussOptions) -> Result<DiscussReport>` | `DiscussReport` | `repo` (collab store) |
| git-bridge | `git_bridge::import(&Ctx, …)` · `export(&Ctx, …)` | `BridgeReport` | `core::git_bridge` (X-bridge, from `cli/bridge/*`) |
| verify / doctor | `verify(&Ctx, VerifyOptions) -> Result<VerifyReport>` | `VerifyReport` | new `verification` (X-verify) |

`try` (sandboxed child run) is a CLI orchestration verb, not a facade op: it
shells a child process and passes its exit code through (today `try_cmd.rs:278`).
It returns a typed `ChildOutcome { exit_code }` that `main()` maps (decision 6),
rather than calling `process::exit` mid-stack.

### 4. Render-separation convention

The rule every extraction follows: **logic returns a typed, `serde`-`Serialize`
data struct; the CLI owns ALL rendering — text, json, json-compact — and ALL
styling.**

- **Result types live in `heddle-core`** (e.g. `crates/core/src/status.rs`
  defines `StatusReport`). They derive `Serialize` (and `Deserialize` where round-
  tripping helps tests). Because they serialize, `--output json` collapses to
  `serde_json::to_writer(stdout, &report)` — JSON output becomes near-free and the
  `schemas` command derives its JSON Schema from the same structs.
- **Renderers live in `cli`**, under `crates/cli/src/render/` (one module per
  command, e.g. `render::status_text`). Text rendering uses the existing `style`
  helpers; the renderer is the *only* place `println!`/`style::*` appears for that
  command.
- The boundary is mechanical: a reviewer can grep `heddle-core` for `println!`,
  `style`, `serde_json::to_string`-for-display, or `OutputMode` and expect zero
  hits; all of those belong to `cli::render`.

Shown for `status` in decision 2's after-block: `status()` returns
`StatusReport`; `render::status_text` / `render::status_json` consume it.

### 5. Progress + structured-warning channels (replacing logic-crate prints)

Two traits, **defined in `crates/objects`** — the lowest crate that every
print-emitting logic crate (`semantic`, `ingest`, `refs`, `oplog`) already depends
on, and the crate that already owns `HeddleError` (`objects/src/error.rs:8`).
Defining them here avoids a dependency cycle (they must live *below* the domain
crates, so they cannot live in `heddle-core`, which depends on those crates) and
avoids minting a new crate. `heddle-core` and `cli` re-export them.

```rust
// crates/objects/src/observe.rs
pub trait ProgressSink: Send + Sync {
    fn event(&self, ev: ProgressEvent);
}
pub enum ProgressEvent {
    Start   { id: TaskId, label: Cow<'static, str>, total: Option<u64> },
    Advance { id: TaskId, delta: u64 },
    Message { id: TaskId, msg: Cow<'static, str> },
    Finish  { id: TaskId },
}
pub trait WarningSink: Send + Sync {
    fn warn(&self, w: Warning);
}
pub struct Warning {
    /// Stable machine-readable class, e.g. "refs_unlock_failed",
    /// "oplog_truncation_recovered". Mirrors the error `kind` discriminator
    /// convention so agents key on it, not on prose.
    pub kind: Cow<'static, str>,
    pub message: String,
}

/// Default for embedders that don't care about progress.
pub struct NoopProgress;
impl ProgressSink for NoopProgress { fn event(&self, _: ProgressEvent) {} }
```

- **Threading:** logic functions that today print take `&dyn ProgressSink` /
  `&dyn WarningSink` (sourced from `ctx.progress()` / `ctx.warnings()`), or take
  `&ExecutionContext` directly for the orchestration-level calls. The
  `refs_storage.rs:33` `Drop` warning and the `packed_oplog.rs:1596` corruption
  warning become `warnings.warn(Warning { kind: …, message: … })`. The `Drop`
  case (which has no obvious place to thread a sink) gets the sink stored on the
  guard struct when it is constructed, or falls back to a `tracing::warn!` event
  the CLI's subscriber renders — never a bare `eprintln!`.
- **No-op for embedders:** pass `Arc::new(NoopProgress)` and a collecting
  `WarningSink`. An embedder running headless never emits a byte to stderr from
  library code.
- **CLI sinks:** `cli` implements a progress sink that drives a stderr bar and a
  warning sink that formats to stderr — the *only* place these become text.

### 6. `process::exit` removal + error → exit-code mapping

- **Rule:** the facade and every command body return `Result<_, HeddleError>`
  (the CLI boundary may widen to `anyhow::Error`). **Only `main()` maps an error
  to an exit code and calls `process::exit`.** The 7 production CLI `process::exit`
  sites inside command/orchestration bodies (`merge/mod.rs:263`,
  `operator_core.rs:248`, `thread_approval.rs:331`, `snapshot.rs:216`,
  `operation_id.rs:147/193`, plus the post-command ones in `main.rs:255/264/277`)
  become `return Err(…)`. The genuine bootstrap exits in `main()`
  (`main.rs:169/172/217`, parse/config failures before a command runs) stay — they
  *are* `main`.
- **The child-exit-passthrough** (`try_cmd.rs:278`) does not vanish: the op
  returns `ChildOutcome { exit_code }` and `main()` performs the single
  `process::exit(exit_code)`. No mid-stack process control.
- **Where the taxonomy lives:** `HeddleExitCode` stays in **`cli`**
  (`crates/cli/src/exit.rs:19`) and must NOT move into `heddle-core` — a library
  has no business owning process lifecycle. `main()` calls
  `HeddleExitCode::from_error(&err)` (`exit.rs:86`), which already walks the error
  chain and classifies on the stable `kind` discriminator (`exit.rs:59`). The
  facade's job is to return errors carrying those `kind`s (the `RecoveryAdvice`
  convention); the CLI's job is to map them. This keeps the BSD `sysexits.h`
  contract intact while removing exits from the library.

### 7. De-singleton plan

| Singleton (today) | Moves to | Re-entrancy win |
|---|---|---|
| `USER_CONFIG` OnceLock (`cli/mod.rs:49`) | `ExecutionContext.config` field, loaded once at construction | two contexts → two configs in one process |
| color `COLOR_STATE` (`style.rs:44`) | **stays in CLI** as render-layer state — a `StyleMode` value passed to renderers, not a process static. Never enters `heddle-core`. | parallel renders don't share a global color bit |
| lazy-hydrator `REGISTRY` (`repo/lazy_hydrator.rs:196`) | **`Repository` field**, populated at `Repository::open` | two repos → two hydrator registries; the headline re-entrancy fix |
| fault-inject `FAULT_POINTS` (`objects/fault_inject.rs:43`) | `ExecutionContext.faults` (`FaultConfig`), env-seeded by the CLI constructor only | embedders never inherit a test env var; faults are per-context |
| semantic `CACHE` (`semantic/cache.rs:57`) | `ExecutionContext.semantic_cache: Arc<SemanticCache>` | content-addressed, so it MAY be shared, but lifetime/bound is now the caller's choice, not a leaked global |

The re-entrancy acceptance test: open two repositories in one process, run a
status (or merge) against each, and assert no cross-talk through any former
singleton.

### 8. Sequencing within 0.5.0

**The campaign ships *inside* the 0.5.0 minor bump; the 0.5.0 stable tag is HELD
until the facade lands.** The plan's Option A ("cut 0.5.0 now, facade on a
0.6.0-dev line") is explicitly rejected by the maintainer. Mechanics: `main` is
the integration line; every WU merges to `main`; **PR #769 (the version bump +
CHANGELOG) is the FINAL pre-tag PR**, rebased on accumulated `main`, and the
0.5.0 tag is cut only after it merges.

Dependency order of the waves (WU ids from `reviews/heddle-hardening-plan.md`):

```
S1 (this ADR) ──────────────► gates the FACADE track only
                              (Tracks 1 & 2 do NOT depend on it)

Phase 1  (parallel NOW, independent of S1, up to the 3-builder cap):
  U-gate · U-repo · U-objects · U-cli · U-small · U-locks   (panic-free)
  Z2 (diff streaming) · Z4 (status string-clones)           (isolated zero-copy)

SPINE PR  (after S1; the serial gate for all of Phase 3):
  = F1 (ExecutionContext spine, ~100 cmd signatures: &Cli → &ExecutionContext)
  + F2 (process::exit → Result; exit-code map confined to main)
  + render-separation seam: create the `heddle-core` crate skeleton with
    ExecutionContext, define the *Report convention + cli::render module, and
    land ProgressSink/WarningSink in crates/objects (decision 5).
  Land as ONE focused PR; freeze parallel cli edits during its window.

Phase 2  (after S1; foundational):
  Z1 (borrow-capable object read)         — sequence AFTER Phase-1 objects work
  F3 (de-singleton; 3 disjoint sub-units) — parallel-safe with each other
  P-print (logic-crate print removal)     — DEPENDS on the spine's sink traits

Phase 3  (after the SPINE PR; fan out, disjoint by domain):
  X-bridge · X-ops · X-diff · X-status · X-verify
  then X-facade assembles heddle-core over the extracted pieces.

Phase 3.5:  Z3 (after Z2 + X-diff) · Z5 (after Z1 + X-ops)

FINAL:  PR #769 (version bump + CHANGELOG) rebased on accumulated main ─► tag 0.5.0
```

**The spine PR is the minimal seam that unblocks parallel domain extraction.** Its
scope is exactly F1 + F2 + the render/observability seam (crate skeleton +
`*Report` convention + `cli::render` + sink traits). Until it lands, no X-* wave
can begin, because they have no `ExecutionContext` to receive and no result-type
convention to follow. Once it lands, the X-* waves are highly parallel because
their file scopes are disjoint by domain.

## Consequences

- An embedder (Weft, a daemon, a test harness) can drive heddle operations,
  receive typed results, capture warnings, and choose its own rendering — with no
  `clap`, no terminal assumptions, and no process exits.
- `--output json` and the `schemas` command derive from the same `Serialize`
  structs, so the json contract and its schema cannot drift from the data the
  logic produces.
- Two operations / two repositories can run in one process — the prerequisite for
  any server or batched embedder.
- The CLI shrinks toward "parse args → build context → call op → render result →
  map error to exit code," with the ~17–19k LOC of trapped logic relocated to
  `heddle-core` and the domain crates.
- Cost: the F1 spine touches ~100 command signatures and is merge-conflict-prone;
  it must land as one frozen-window PR. The `Z1` borrow API and the de-singleton
  moves ripple through every `ObjectStore`/`Repository` construction site.

## Open questions / risks

- **`Repository` clone cost.** The context holds `Option<Repository>`; this
  assumes the handle is `Arc`-backed and cheap to clone/hold. If it is not, the
  context should hold `Arc<Repository>` — confirm during F1.
- **`Drop`-site warnings.** `refs_storage.rs:33` warns from a `Drop` impl where no
  sink is in scope. Resolution: store the sink (or fall back to a `tracing::warn!`
  the CLI subscriber renders) on the guard at construction. If neither is clean,
  the structured `tracing` event is the accepted fallback — never `eprintln!`.
- **Observability-trait home.** `crates/objects` is chosen as the lowest common
  dependency; if a future refactor lifts `HeddleError` and these traits into a
  dedicated tiny `heddle-observe` crate, `heddle-core` re-exports shield callers
  from the move.
- **Async surface.** `status` is `async` today; `merge` is sync. The facade should
  pick one convention per op based on its real I/O; mixed sync/async across the op
  table is accepted rather than forcing a uniform async wrapper.
- **Cancellation.** A `CancellationToken` on the context is deferred to when a
  long-running embedder needs it; not in the v1 field set.
- **`verbosity` vs log-level.** The context's `verbosity` is a semantic detail
  hint; the `tracing` subscriber's level stays a CLI concern initialized in
  `main()`. Keep the two from fusing.

**Status:** proposed

**Considered Options:** (a) *Extend `cli-shared` or `repo` instead of a new
crate* — rejected: it inflates a config crate's dependency surface or forces a
domain crate to depend on its siblings, risking cycles. (b) *Keep `OutputMode`/
color in the `ExecutionContext`* (the plan's F1 sketch) — rejected: output mode is
a render selection an embedder never needs, and keeping it in the facade weakens
the boundary. (c) *Define the progress/warning traits in `heddle-core`* — rejected:
the logic crates that must report live *below* `heddle-core`, so the traits would
create a cycle; `crates/objects` is the lowest shared home. (d) *Cut 0.5.0 now and
build the facade on a 0.6.0-dev line* (plan Option A) — rejected by the maintainer:
the campaign is folded into 0.5.0 and the tag is held until the facade lands.
